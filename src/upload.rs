//! Chunked uploads, for files larger than the hosting proxy lets one request carry (100 MB).
//!
//! `PUT /<path>` with `Content-Range: bytes a-b/total` stores part [a, b] in a session under
//! `DATA_DIR/.uploads/<sha256 of path>/` (`<a>-<b>.part` files and `manifest.json` with the path,
//! total, received ranges and start time). Parts arrive in any order; sending the same range again
//! overwrites it. `POST /<path>?complete=1` with `X-Content-Sha256` joins the parts in order into a
//! temp file beside the target, checks the length and digest, and renames it into place.
//! `DELETE /<path>?upload=1` aborts. Sessions older than 24 hours are swept.
use crate::cache::siblings;
use crate::sha256::{self, Sha256};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Largest part one request may carry.
pub const PART_MAX: u64 = 100 * 1024 * 1024;
/// Sessions not completed within this many seconds are swept.
pub const TTL_SECS: u64 = 24 * 3600;
/// Directory inside DATA_DIR that holds the sessions (never served, never indexed).
pub const DIR: &str = ".uploads";

/// A refusal: status code and JSON body.
pub type Fail = (StatusCode, Value);

fn fail(code: StatusCode, msg: impl Into<String>) -> Fail {
    (code, json!({ "error": msg.into() }))
}

fn internal(e: io::Error) -> Fail {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// `bytes a-b/total` as (a, b, total); `*` totals and anything else malformed fail.
pub fn content_range(v: &str) -> Option<(u64, u64, u64)> {
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    Some((num(a)?, num(b)?, num(total)?))
}

/// The expected digest from `X-Content-Sha256`: 64 hex digits, any case.
pub fn digest(v: &str) -> Option<String> {
    let v = v.trim().to_ascii_lowercase();
    (v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit())).then_some(v)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub path: String,
    pub total: u64,
    pub started: u64,
    /// Received inclusive ranges, sorted, never overlapping.
    pub ranges: Vec<(u64, u64)>,
    /// A complete request is joining the parts.
    pub completing: bool,
}

impl Manifest {
    fn json(&self) -> Value {
        json!({
            "path": self.path, "total": self.total, "started": self.started,
            "ranges": self.ranges.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>(),
            "completing": self.completing,
        })
    }

    fn parse(v: &Value) -> Option<Manifest> {
        let ranges = v["ranges"]
            .as_array()?
            .iter()
            .map(|r| Some((r[0].as_u64()?, r[1].as_u64()?)))
            .collect::<Option<Vec<_>>>()?;
        Some(Manifest {
            path: v["path"].as_str()?.into(),
            total: v["total"].as_u64()?,
            started: v["started"].as_u64()?,
            ranges,
            completing: v["completing"].as_bool().unwrap_or(false),
        })
    }

    pub fn received(&self) -> u64 {
        self.ranges.iter().map(|(a, b)| b - a + 1).sum()
    }

    /// Byte ranges not received yet.
    pub fn missing(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut next = 0;
        for &(a, b) in &self.ranges {
            if a > next {
                out.push((next, a - 1));
            }
            next = next.max(b + 1);
        }
        if next < self.total {
            out.push((next, self.total - 1));
        }
        out
    }

    /// A different received range that shares bytes with [a, b].
    fn overlap(&self, a: u64, b: u64) -> Option<(u64, u64)> {
        self.ranges
            .iter()
            .copied()
            .find(|&(x, y)| (x, y) != (a, b) && a <= y && x <= b)
    }

    /// What a client sees after each part.
    pub fn status(&self) -> Value {
        json!({
            "path": self.path, "total": self.total, "received": self.received(),
            "ranges": self.ranges.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>(),
            "missing": self.missing().iter().map(|(a, b)| [a, b]).collect::<Vec<_>>(),
        })
    }
}

pub struct Uploads {
    root: PathBuf,
    /// Largest total a session may announce (MAX_BYTES).
    max_total: u64,
    /// Serialises manifest reads and writes (held only for small synchronous file operations).
    lock: Mutex<()>,
}

impl Uploads {
    pub fn new(data_dir: &Path, max_total: u64) -> Uploads {
        Uploads {
            root: data_dir.join(DIR),
            max_total,
            lock: Mutex::new(()),
        }
    }

    fn session(&self, rel: &str) -> PathBuf {
        self.root.join(sha256::hex(rel.as_bytes()))
    }

    fn read(dir: &Path) -> Option<Manifest> {
        let b = std::fs::read(dir.join("manifest.json")).ok()?;
        Manifest::parse(&serde_json::from_slice(&b).ok()?)
    }

    fn write(dir: &Path, m: &Manifest) -> io::Result<()> {
        let tmp = dir.join("manifest.json.tmp");
        std::fs::write(&tmp, m.json().to_string())?;
        std::fs::rename(tmp, dir.join("manifest.json"))
    }

    /// Store part [a, b] of `total` for `rel`; `len` is the request's Content-Length if any.
    pub async fn part(
        &self,
        rel: &str,
        (a, b, total): (u64, u64, u64),
        len: Option<u64>,
        mut body: impl AsyncRead + Unpin,
    ) -> Result<Value, Fail> {
        if a > b || b >= total {
            return Err(fail(
                StatusCode::RANGE_NOT_SATISFIABLE,
                format!("range {a}-{b} is not inside 0-{}", total.saturating_sub(1)),
            ));
        }
        let size = b - a + 1;
        if size > PART_MAX {
            return Err(fail(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("part is {size} bytes, at most {PART_MAX}"),
            ));
        }
        if total > self.max_total {
            return Err(fail(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("total is {total} bytes, at most {}", self.max_total),
            ));
        }
        if len.is_some_and(|l| l != size) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                format!("Content-Length does not match the {size} bytes of {a}-{b}"),
            ));
        }
        let dir = self.session(rel);
        {
            let _g = self.lock.lock().unwrap();
            let m = match Self::read(&dir) {
                Some(m) => m,
                None => {
                    self.sweep_locked(false);
                    let m = Manifest {
                        path: rel.into(),
                        total,
                        started: now(),
                        ranges: Vec::new(),
                        completing: false,
                    };
                    std::fs::create_dir_all(&dir).map_err(internal)?;
                    Self::write(&dir, &m).map_err(internal)?;
                    eprintln!("upload {rel}: session started, {total} bytes");
                    m
                }
            };
            check(&m, a, b, total)?;
        }
        let tmp = dir.join(format!(".{a}-{b}.{}.tmp", nanos()));
        let res = async {
            let mut f = tokio::fs::File::create(&tmp).await.map_err(internal)?;
            let n = tokio::io::copy(&mut (&mut body).take(size + 1), &mut f)
                .await
                .map_err(|e| fail(StatusCode::BAD_REQUEST, format!("reading the part: {e}")))?;
            if n != size {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "body is {n}{} bytes, {a}-{b} is {size}",
                        if n > size { "+" } else { "" }
                    ),
                ));
            }
            f.flush().await.map_err(internal)?;
            f.sync_all().await.map_err(internal)?;
            let _g = self.lock.lock().unwrap();
            let Some(mut m) = Self::read(&dir) else {
                return Err(fail(StatusCode::CONFLICT, "upload aborted meanwhile"));
            };
            check(&m, a, b, total)?;
            std::fs::rename(&tmp, dir.join(format!("{a}-{b}.part"))).map_err(internal)?;
            if !m.ranges.contains(&(a, b)) {
                m.ranges.push((a, b));
                m.ranges.sort_unstable();
            }
            Self::write(&dir, &m).map_err(internal)?;
            Ok(m.status())
        }
        .await;
        if res.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        res
    }

    /// Claim a fully received session for joining. Fails if parts are missing.
    pub fn begin_complete(&self, rel: &str) -> Result<Manifest, Fail> {
        let dir = self.session(rel);
        let _g = self.lock.lock().unwrap();
        let Some(mut m) = Self::read(&dir) else {
            return Err(fail(
                StatusCode::NOT_FOUND,
                "no upload session for this path",
            ));
        };
        if m.completing {
            return Err(fail(StatusCode::CONFLICT, "already completing"));
        }
        let missing = m.missing();
        if !missing.is_empty() {
            let mut v = m.status();
            v["error"] = json!("parts missing");
            return Err((StatusCode::CONFLICT, v));
        }
        m.completing = true;
        Self::write(&dir, &m).map_err(internal)?;
        Ok(m)
    }

    /// Join the parts of `m` into `file` (blocking). Parts are consumed as they are copied, so the
    /// disk holds one copy; the session is gone afterwards, whatever the outcome. Returns the size.
    pub fn assemble(&self, m: &Manifest, file: &Path, want: &str) -> Result<u64, Fail> {
        let dir = self.session(&m.path);
        let parent = file.parent().expect("file in bucket");
        let tmp = parent.join(format!(".upload-{}-{}", std::process::id(), nanos()));
        let res = (|| {
            std::fs::create_dir_all(parent).map_err(internal)?;
            let mut out = std::fs::File::create(&tmp).map_err(internal)?;
            let (mut sha, mut n) = (Sha256::default(), 0u64);
            let mut buf = vec![0u8; 1 << 20];
            for &(a, b) in &m.ranges {
                let part = dir.join(format!("{a}-{b}.part"));
                let mut f = std::fs::File::open(&part).map_err(internal)?;
                loop {
                    let k = f.read(&mut buf).map_err(internal)?;
                    if k == 0 {
                        break;
                    }
                    sha.update(&buf[..k]);
                    out.write_all(&buf[..k]).map_err(internal)?;
                    n += k as u64;
                }
                let _ = std::fs::remove_file(&part);
            }
            if n != m.total {
                return Err(fail(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("joined {n} bytes, expected {}", m.total),
                ));
            }
            let got = sha.hex();
            if got != want {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    json!({"error": "sha256 mismatch", "expected": want, "got": got}),
                ));
            }
            out.sync_all().map_err(internal)?;
            std::fs::rename(&tmp, file).map_err(internal)?;
            Ok(n)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        let _g = self.lock.lock().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        res
    }

    /// Drop the session for `rel`. True if there was one.
    pub fn abort(&self, rel: &str) -> Result<bool, Fail> {
        let dir = self.session(rel);
        let _g = self.lock.lock().unwrap();
        match Self::read(&dir) {
            Some(m) if m.completing => Err(fail(StatusCode::CONFLICT, "already completing")),
            _ if dir.exists() => {
                std::fs::remove_dir_all(&dir).map_err(internal)?;
                eprintln!("upload {rel}: aborted");
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Remove sessions older than [`TTL_SECS`] (and, at startup, ones a crash left completing).
    pub fn sweep(&self, startup: bool) -> Vec<String> {
        let _g = self.lock.lock().unwrap();
        self.sweep_locked(startup)
    }

    fn sweep_locked(&self, startup: bool) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut gone = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            let (name, stale) = match Self::read(&p) {
                Some(m) => (
                    m.path.clone(),
                    now().saturating_sub(m.started) > TTL_SECS || (startup && m.completing),
                ),
                None => {
                    // No manifest: half-created; judge by the directory's age.
                    let age = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map_or(0, |d| d.as_secs());
                    (e.file_name().to_string_lossy().into_owned(), age > TTL_SECS)
                }
            };
            if stale && std::fs::remove_dir_all(&p).is_ok() {
                eprintln!("upload {name}: session swept");
                gone.push(name);
            }
        }
        gone
    }
}

fn check(m: &Manifest, a: u64, b: u64, total: u64) -> Result<(), Fail> {
    if m.completing {
        return Err(fail(StatusCode::CONFLICT, "already completing"));
    }
    if m.total != total {
        return Err(fail(
            StatusCode::CONFLICT,
            format!("session total is {}, not {total}; abort it first", m.total),
        ));
    }
    if let Some((x, y)) = m.overlap(a, b) {
        return Err(fail(
            StatusCode::CONFLICT,
            format!("{a}-{b} overlaps received part {x}-{y}"),
        ));
    }
    Ok(())
}

/// After `rel` (an ISO) is stored: keep the `keep` newest ISOs in its directory, delete the older
/// ones with their `.sha256`/`.sig` siblings. `keep` 0 keeps all. Returns the removed paths.
pub fn keep_latest(root: &Path, rel: &str, keep: usize) -> Vec<String> {
    if keep == 0 || !rel.ends_with(".iso") {
        return Vec::new();
    }
    let (dir_rel, _) = rel.rsplit_once('/').unwrap_or(("", rel));
    let dir = root.join(dir_rel);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let join = |n: &str| {
        if dir_rel.is_empty() {
            n.to_string()
        } else {
            format!("{dir_rel}/{n}")
        }
    };
    let mut isos: Vec<(bool, SystemTime, String)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let m = e.metadata().ok()?;
            (name.ends_with(".iso") && m.is_file()).then(|| {
                let r = join(&name);
                (r == rel, m.modified().unwrap_or(UNIX_EPOCH), r)
            })
        })
        .collect();
    // The one just stored first, then newest first (name breaks ties).
    isos.sort_by(|x, y| y.0.cmp(&x.0).then(y.1.cmp(&x.1)).then(y.2.cmp(&x.2)));
    let mut gone = Vec::new();
    for (_, _, old) in isos.into_iter().skip(keep) {
        for f in std::iter::once(old.clone()).chain(siblings(&old)) {
            if std::fs::remove_file(root.join(&f)).is_ok() {
                eprintln!("keep-latest: removed {f} (newer ISO {rel})");
                gone.push(f);
            }
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parses_strictly() {
        assert_eq!(content_range("bytes 0-99/200"), Some((0, 99, 200)));
        assert_eq!(content_range(" bytes 100-199/200 "), Some((100, 199, 200)));
        for bad in [
            "bytes 0-99/*",
            "bytes */200",
            "0-99/200",
            "bytes -1-99/200",
            "bytes 0-/200",
            "bytes +0-9/20",
            "items 0-9/20",
        ] {
            assert_eq!(content_range(bad), None, "{bad}");
        }
    }

    #[test]
    fn digests_are_64_hex() {
        let d = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        assert_eq!(digest(d).as_deref(), Some(d.to_lowercase().as_str()));
        assert_eq!(digest("abc"), None);
        assert_eq!(digest(&"g".repeat(64)), None);
    }

    #[test]
    fn missing_ranges_and_overlap() {
        let mut m = Manifest {
            path: "a".into(),
            total: 30,
            started: 0,
            ranges: vec![(10, 19)],
            completing: false,
        };
        assert_eq!(m.missing(), vec![(0, 9), (20, 29)]);
        assert_eq!(m.overlap(10, 19), None, "same range is a retry");
        assert_eq!(m.overlap(15, 24), Some((10, 19)));
        assert_eq!(m.overlap(0, 9), None);
        m.ranges = vec![(0, 9), (10, 19), (20, 29)];
        assert!(m.missing().is_empty());
        assert_eq!(m.received(), 30);
        assert_eq!(Manifest::parse(&m.json()), Some(m));
    }
}
