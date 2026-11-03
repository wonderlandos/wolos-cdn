//! The releases index: every top-level `DATA_DIR/v<semver>/` folder with a `release.json` is a
//! release. It is complete when it also holds an `.iso` with its `.sha256` (`x.iso.sha256` or
//! `x.sha256`); a folder whose ISO went (KEEP_ISOS, the disk budget) stays listed with `iso` null
//! and the GitHub asset URL (`image` in its release.json) as `mirror`. A folder with an ISO but
//! no valid checksum is still being published and is left out. Newest version first (semver).
//!
//! `GET /releases.json`, `/releases/latest.json` and the `/releases` page read one in-memory copy;
//! any write, delete or eviction under a `v*/` folder drops it, and the next read builds it again.
use crate::cache::siblings;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// A semantic version: `MAJOR.MINOR.PATCH`, an optional `-pre.release`, and `+build` (ignored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let ident =
            |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
        let (s, build) = s.split_once('+').map_or((s, None), |(a, b)| (a, Some(b)));
        if build.is_some_and(|b| !b.split('.').all(ident)) {
            return None;
        }
        let (core, pre) = s.split_once('-').map_or((s, None), |(a, b)| (a, Some(b)));
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return None;
        }
        let mut c = [0u64; 3];
        for (slot, n) in c.iter_mut().zip(&nums) {
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *slot = n.parse().ok()?;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) if p.split('.').all(ident) => p.split('.').map(String::from).collect(),
            Some(_) => return None,
        };
        Some(Version { core: c, pre })
    }
}

impl Ord for Version {
    /// Semver precedence: core numbers, then a release above its pre-releases, then the
    /// pre-release identifiers (numbers numerically and below words, words by ASCII).
    fn cmp(&self, o: &Self) -> Ordering {
        let pre = || match (self.pre.is_empty(), o.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&o.pre) {
                    let c = match (a.parse::<u64>(), b.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => a.cmp(b),
                    };
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                self.pre.len().cmp(&o.pre.len())
            }
        };
        self.core.cmp(&o.core).then_with(pre)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// The version of a release folder name (`v0.12.0`), if it is one.
pub fn folder_version(name: &str) -> Option<Version> {
    Version::parse(name.strip_prefix('v')?)
}

/// True when `rel` lies inside a top-level release folder (`v0.12.0/x.iso`).
pub fn in_release(rel: &str) -> bool {
    rel.split_once('/')
        .is_some_and(|(first, _)| folder_version(first).is_some())
}

/// File names a link and a GET can carry as they are (no escaping needed, `clean` accepts them).
fn plain_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with('.')
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-+~".contains(&c))
}

/// An absolute http(s) URL, or a path on this host; anything else (`javascript:`) is dropped.
fn link(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    let ok = s.starts_with("https://")
        || s.starts_with("http://")
        || (s.starts_with('/') && !s.starts_with("//"));
    (ok && !s
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '<' || c == '>'))
    .then(|| s.to_string())
}

/// A URL to this release folder on this host: a local path, or any host whose path is
/// `/<folder>/<file>` (the CDN layout; GitHub assets live under `/<owner>/<repo>/releases/`).
/// The server does not know its public name, so the path decides.
fn on_this_host(url: &str, folder: &str) -> bool {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
        None => url,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.starts_with('/')
        && (!url.contains("://")
            || path
                .strip_prefix('/')
                .and_then(|p| p.strip_prefix(folder))
                .and_then(|p| p.strip_prefix('/'))
                .is_some_and(|f| !f.is_empty() && !f.contains('/')))
}

fn hex64(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    (s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())).then_some(s)
}

/// The digest at the start of a `sha256sum` line (`<hex>  <name>`).
fn read_sha(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    hex64(s.split_whitespace().next()?)
}

/// `YYYY-MM-DD` (UTC) of a file time.
fn day(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    crate::report::date(secs / 86400)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// The folder, `v0.12.0`.
    pub folder: String,
    version: Version,
    /// Folder name without the `v`: what the site puts after `/v`.
    pub version_s: String,
    pub date: Option<String>,
    /// The ISO's file name in the folder; None once it was evicted.
    pub iso: Option<String>,
    /// The checksum file beside the ISO (`x.iso.sha256` or `x.sha256`).
    pub sha_file: Option<String>,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub notes_url: Option<String>,
    /// The GitHub release asset (`image` in release.json).
    pub mirror: Option<String>,
}

impl Release {
    pub fn json(&self) -> Value {
        json!({
            "version": self.version_s,
            "date": self.date,
            "iso": self.iso,
            "size": self.size,
            "sha256": self.sha256,
            "release_json": format!("/{}/release.json", self.folder),
            "notes_url": self.notes_url,
            "mirror": self.mirror,
        })
    }
}

/// One release folder, or None if it is not (yet) a release.
fn read_release(dir: &Path, folder: &str, version: Version) -> Option<Release> {
    let d = dir.join(folder);
    let rj_path = d.join("release.json");
    let rj: Value = serde_json::from_slice(&std::fs::read(&rj_path).ok()?).ok()?;
    if !rj.is_object() {
        return None;
    }
    // `image` is this host's copy (`https://<cdn>/v0.12.0/x.iso`), `mirror` the GitHub one; an
    // older release.json has only `image`, pointing at GitHub.
    let image = link(&rj["image"]);
    let mirror = link(&rj["mirror"])
        .filter(|m| !on_this_host(m, folder))
        .or_else(|| image.clone().filter(|i| !on_this_host(i, folder)));
    let want = image
        .as_deref()
        .or(mirror.as_deref())
        .and_then(|m| m.rsplit('/').next())
        .map(String::from);
    // The ISO release.json names, else the newest one in the folder.
    let mut isos: Vec<(bool, SystemTime, String, u64)> = std::fs::read_dir(&d)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let m = e.metadata().ok()?;
            (name.ends_with(".iso") && plain_name(&name) && m.is_file()).then(|| {
                let t = m.modified().unwrap_or(UNIX_EPOCH);
                (want.as_deref() == Some(name.as_str()), t, name, m.len())
            })
        })
        .collect();
    isos.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    let given_date = rj["date"].as_str().map(String::from);
    let notes_url = link(&rj["notes_url"]);
    let base = Release {
        folder: folder.into(),
        version_s: folder[1..].into(),
        version,
        date: None,
        iso: None,
        sha_file: None,
        size: None,
        sha256: None,
        notes_url,
        mirror,
    };
    match isos.into_iter().next() {
        Some((_, t, name, size)) => {
            let (sha_file, sha) = siblings(&name)
                .into_iter()
                .filter(|s| s.ends_with(".sha256"))
                .find_map(|s| read_sha(&d.join(&s)).map(|h| (s, h)))?;
            Some(Release {
                date: given_date.or_else(|| Some(day(t))),
                iso: Some(name),
                sha_file: Some(sha_file),
                size: Some(size),
                sha256: Some(sha),
                ..base
            })
        }
        None => {
            let t = std::fs::metadata(&rj_path).and_then(|m| m.modified()).ok();
            Some(Release {
                date: given_date.or_else(|| t.map(day)),
                size: rj["size"].as_u64(),
                sha256: rj["sha256"].as_str().and_then(hex64),
                ..base
            })
        }
    }
}

/// Every release under `dir`, newest version first.
pub fn scan(dir: &Path) -> Vec<Release> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Release> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let v = folder_version(&name)?;
            read_release(dir, &name, v)
        })
        .collect();
    out.sort_by(|a, b| b.version.cmp(&a.version).then(b.folder.cmp(&a.folder)));
    out
}

pub struct Index {
    pub list: Vec<Release>,
    /// `{"releases": [...]}`, serialised once.
    pub json: String,
}

pub struct Releases {
    dir: PathBuf,
    built: Mutex<Option<Arc<Index>>>,
}

impl Releases {
    pub fn new(dir: &Path) -> Releases {
        Releases {
            dir: dir.into(),
            built: Mutex::new(None),
        }
    }

    /// The index, built now if a change dropped it.
    pub fn get(&self) -> Arc<Index> {
        let mut g = self.built.lock().unwrap();
        if let Some(i) = &*g {
            return i.clone();
        }
        let list = scan(&self.dir);
        let json = json!({"releases": list.iter().map(Release::json).collect::<Vec<_>>()});
        eprintln!("releases: index built, {} release(s)", list.len());
        let i = Arc::new(Index {
            json: json.to_string(),
            list,
        });
        *g = Some(i.clone());
        i
    }

    /// `rel` was written, removed or evicted: under a release folder, the index is built again.
    pub fn changed(&self, rel: &str) {
        if in_release(rel) {
            *self.built.lock().unwrap() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn semver_precedence() {
        let order = [
            "0.9.1",
            "0.11.4",
            "0.12.0-alpha",
            "0.12.0-alpha.1",
            "0.12.0-alpha.beta",
            "0.12.0-beta.2",
            "0.12.0-beta.11",
            "0.12.0-rc.1",
            "0.12.0",
            "1.0.0",
        ];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("1.2.3+build.5").cmp(&v("1.2.3")), Ordering::Equal);
        for bad in [
            "1.2",
            "1.2.3.4",
            "a.b.c",
            "1.2.3-",
            "1.2.3-a..b",
            "1..3",
            "",
            "1.2.3+",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(folder_version("v0.12.0").is_some());
        assert!(folder_version("0.12.0").is_none() && folder_version("vx").is_none());
        assert!(in_release("v0.12.0/a.iso") && !in_release("v0.12.0") && !in_release("iso/a.iso"));
    }

    #[test]
    fn own_copies_are_not_mirrors() {
        let gh = "https://github.com/wonderlandos/wolos-packages/releases/download/v0.12.0/x.iso";
        assert!(!on_this_host(gh, "v0.12.0"));
        assert!(on_this_host(
            "https://wolos.squareweb.app/v0.12.0/x.iso",
            "v0.12.0"
        ));
        assert!(on_this_host(
            "http://127.0.0.1:8080/v0.12.0/x.iso?dl=1",
            "v0.12.0"
        ));
        assert!(on_this_host("/v0.12.0/x.iso", "v0.12.0"));
        assert!(!on_this_host(
            "https://cdn.example/v0.11.4/x.iso",
            "v0.12.0"
        ));
        assert!(!on_this_host(
            "https://cdn.example/a/v0.12.0/x.iso",
            "v0.12.0"
        ));
    }

    #[test]
    fn links_are_http_or_local() {
        assert_eq!(link(&json!("https://x/y")).as_deref(), Some("https://x/y"));
        assert_eq!(link(&json!("/blog/a")).as_deref(), Some("/blog/a"));
        for bad in [
            json!("javascript:alert(1)"),
            json!("//evil"),
            json!("https://a b"),
            json!(3),
        ] {
            assert_eq!(link(&bad), None, "{bad}");
        }
    }
}
