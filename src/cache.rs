//! Disk budget: every file in the bucket with its size and last use. Over the budget, the least
//! recently used immutable files go first; they can always be fetched again (upstream mirror or the
//! release assets). Databases and manifests are small and never evicted.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Cache {
    dir: PathBuf,
    pub max: u64,
    files: Mutex<HashMap<String, Entry>>,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub sent: AtomicU64,
}

#[derive(Clone, Copy)]
struct Entry {
    size: u64,
    used: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Package files and ISOs never change under one name; databases, manifests and signatures do.
pub fn immutable(rel: &str) -> bool {
    rel.contains(".pkg.tar.") || rel.ends_with(".iso")
}

impl Cache {
    /// Index what is already on disk (last use = modification time).
    pub fn open(dir: &Path, max: u64) -> Cache {
        let mut files = HashMap::new();
        scan(dir, dir, &mut files);
        Cache {
            dir: dir.into(),
            max,
            files: Mutex::new(files),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            sent: AtomicU64::new(0),
        }
    }

    pub fn used(&self) -> u64 {
        self.files.lock().unwrap().values().map(|e| e.size).sum()
    }

    pub fn count(&self) -> usize {
        self.files.lock().unwrap().len()
    }

    pub fn touch(&self, rel: &str) {
        if let Some(e) = self.files.lock().unwrap().get_mut(rel) {
            e.used = now();
        }
    }

    pub fn forget(&self, rel: &str) {
        self.files.lock().unwrap().remove(rel);
    }

    /// Record a new file, then evict until the bucket fits. Returns the evicted paths.
    pub fn added(&self, rel: &str, size: u64) -> Vec<String> {
        let mut files = self.files.lock().unwrap();
        files.insert(rel.into(), Entry { size, used: now() });
        let mut total: u64 = files.values().map(|e| e.size).sum();
        let mut old: Vec<(String, Entry)> = files
            .iter()
            .filter(|(k, _)| immutable(k) && k.as_str() != rel)
            .map(|(k, e)| (k.clone(), *e))
            .collect();
        old.sort_by_key(|(_, e)| e.used);
        let mut gone = Vec::new();
        for (k, e) in old {
            if total <= self.max {
                break;
            }
            if std::fs::remove_file(self.dir.join(&k)).is_ok() {
                files.remove(&k);
                total -= e.size;
                gone.push(k);
            }
        }
        gone
    }

    /// Most recently used files, newest first.
    pub fn recent(&self, n: usize) -> Vec<(String, u64, u64)> {
        let files = self.files.lock().unwrap();
        let mut v: Vec<_> = files
            .iter()
            .map(|(k, e)| (k.clone(), e.size, e.used))
            .collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.2));
        v.truncate(n);
        v
    }

    pub fn hit(&self, bytes: u64, fresh: bool) {
        let c = if fresh { &self.misses } else { &self.hits };
        c.fetch_add(1, Ordering::Relaxed);
        self.sent.fetch_add(bytes, Ordering::Relaxed);
    }
}

fn scan(root: &Path, dir: &Path, out: &mut HashMap<String, Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() {
            scan(root, &p, out);
        } else if !e.file_name().to_string_lossy().starts_with(".upload-") {
            let used = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            out.insert(
                rel,
                Entry {
                    size: m.len(),
                    used,
                },
            );
        } else {
            let _ = std::fs::remove_file(&p); // a crash mid-write
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wolos-cdn-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(d: &Path, rel: &str, n: usize) {
        std::fs::write(d.join(rel), vec![0u8; n]).unwrap();
    }

    #[test]
    fn least_recently_used_package_goes_first() {
        let d = dir("lru");
        let c = Cache::open(&d, 250);
        for (i, f) in ["a.pkg.tar.zst", "b.pkg.tar.zst"].iter().enumerate() {
            put(&d, f, 100);
            c.added(f, 100);
            std::thread::sleep(std::time::Duration::from_millis(5 + i as u64));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        c.touch("a.pkg.tar.zst"); // a is used again: b is now the oldest
        put(&d, "c.pkg.tar.zst", 100);
        assert_eq!(c.added("c.pkg.tar.zst", 100), vec!["b.pkg.tar.zst"]);
        assert!(!d.join("b.pkg.tar.zst").exists());
        assert_eq!(c.used(), 200);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn databases_stay_and_the_new_file_stays() {
        let d = dir("keep");
        let c = Cache::open(&d, 50);
        put(&d, "wolos.db", 40);
        c.added("wolos.db", 40);
        put(&d, "big.iso", 100);
        assert!(
            c.added("big.iso", 100).is_empty(),
            "nothing evictable but itself"
        );
        assert!(d.join("wolos.db").exists() && d.join("big.iso").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reopen_indexes_disk_and_drops_partial_uploads() {
        let d = dir("scan");
        std::fs::create_dir_all(d.join("x")).unwrap();
        put(&d, "x/a.pkg.tar.zst", 7);
        put(&d, "x/.upload-1-2", 9);
        let c = Cache::open(&d, 100);
        assert_eq!((c.count(), c.used()), (1, 7));
        assert!(!d.join("x/.upload-1-2").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
