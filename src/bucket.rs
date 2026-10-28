//! Request handling: path rules, auth, cache headers, atomic writes, pull-through from ORIGIN.
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

/// Run until the process dies; `workers` threads share one listener.
pub fn serve(addr: &str, cfg: Config, workers: usize) {
    let server = Arc::new(Server::http(addr).expect("listen"));
    let ctx = Arc::new(cfg);
    let threads: Vec<_> = (0..workers.max(1))
        .map(|_| {
            let (server, ctx) = (server.clone(), ctx.clone());
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    handle(req, &ctx);
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
}

pub struct Config {
    pub dir: PathBuf,
    /// Upload token; empty = read only.
    pub token: String,
    /// Base URL a missing file is fetched from (GitHub release downloads); empty = none.
    pub origin: String,
}

/// A URL path as a relative path inside the bucket; directories, `..` and backslashes fail.
pub fn clean(url: &str) -> Option<String> {
    let p = url.split('?').next().unwrap_or("");
    if p.ends_with('/') || p.contains('\\') || p.contains('%') {
        return None; // no percent-encoding in repo names: refuse rather than decode
    }
    let parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

/// Package files and ISOs never change under one name; databases, manifests and signatures do.
pub fn cache_control(rel: &str) -> &'static str {
    if rel.contains(".pkg.tar.") || rel.ends_with(".iso") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn authorized(req: &Request, token: &str) -> bool {
    let want = format!("Bearer {token}");
    !token.is_empty()
        && req.headers().iter().any(|h| {
            h.field.equiv("Authorization") && constant_eq(h.value.as_bytes(), want.as_bytes())
        })
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("ascii header")
}

fn status(req: Request, code: u16, text: &str) {
    let _ = req.respond(Response::from_string(text).with_status_code(code));
}

fn handle(mut req: Request, cfg: &Config) {
    let (dir, token) = (cfg.dir.as_path(), cfg.token.as_str());
    if req.url() == "/healthz" {
        return status(req, 200, "ok\n");
    }
    let Some(rel) = clean(req.url()) else {
        return status(req, 400, "bad path\n");
    };
    let file = dir.join(&rel);
    match req.method() {
        Method::Get | Method::Head => match open_or_pull(&file, &rel, &cfg.origin) {
            Some(Body::File(f)) => {
                let r = Response::from_file(f)
                    .with_header(header("Cache-Control", cache_control(&rel)))
                    .with_header(header("Content-Type", "application/octet-stream"));
                let _ = req.respond(r);
            }
            Some(Body::Fresh(bytes)) => {
                let r = Response::from_data(bytes)
                    .with_header(header("Cache-Control", cache_control(&rel)))
                    .with_header(header("Content-Type", "application/octet-stream"));
                let _ = req.respond(r);
            }
            None => status(req, 404, "not found\n"),
        },
        Method::Put | Method::Delete if !authorized(&req, token) => {
            status(req, 401, "unauthorized\n")
        }
        Method::Put => match put(&mut req, &file) {
            Ok(()) => status(req, 201, ""),
            Err(e) => status(req, 500, &format!("{e}\n")),
        },
        Method::Delete => match fs::remove_file(&file) {
            Ok(()) => status(req, 204, ""),
            Err(e) if e.kind() == io::ErrorKind::NotFound => status(req, 204, ""),
            Err(e) => status(req, 500, &format!("{e}\n")),
        },
        _ => status(req, 405, "method not allowed\n"),
    }
}

enum Body {
    File(fs::File),
    /// Fetched from the origin and not stored (it changes on every publish).
    Fresh(Vec<u8>),
}

/// Local file, else the origin's copy: immutable files are stored for the next request.
fn open_or_pull(file: &Path, rel: &str, origin: &str) -> Option<Body> {
    if let Ok(f) = fs::File::open(file)
        && f.metadata().is_ok_and(|m| m.is_file())
    {
        return Some(Body::File(f));
    }
    if origin.is_empty() {
        return None;
    }
    let resp = ureq::get(&format!("{}/{rel}", origin.trim_end_matches('/')))
        .call()
        .ok()?;
    let mut reader = resp.into_reader();
    if !cache_control(rel).contains("immutable") {
        let mut v = Vec::new();
        io::copy(&mut reader, &mut v).ok()?;
        return Some(Body::Fresh(v));
    }
    store(&mut reader, file).ok()?;
    fs::File::open(file).ok().map(Body::File)
}

fn put(req: &mut Request, file: &Path) -> io::Result<()> {
    store(req.as_reader(), file)
}

/// Write `src` to `file` through a temp file in the same directory, then rename.
fn store(src: &mut dyn io::Read, file: &Path) -> io::Result<()> {
    let parent = file.parent().expect("file in bucket");
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".upload-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let res = (|| {
        let mut f = fs::File::create(&tmp)?;
        io::copy(src, &mut f)?;
        f.flush()?;
        f.sync_all()?;
        fs::rename(&tmp, file)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_bucket() {
        assert_eq!(
            clean("/wolos/x86_64/wolos.db").as_deref(),
            Some("wolos/x86_64/wolos.db")
        );
        assert_eq!(clean("//a/./b").as_deref(), Some("a/b"));
        for bad in ["/../x", "/a/../../x", "/%2e%2e/x", "/", "/a/", "/a\\b", ""] {
            assert_eq!(clean(bad), None, "{bad}");
        }
    }

    #[test]
    fn packages_cache_forever_databases_never() {
        assert!(cache_control("wolos/x86_64/a-1-1-x86_64.pkg.tar.zst").contains("immutable"));
        assert!(cache_control("iso/wolos-2026.11.iso").contains("immutable"));
        assert_eq!(cache_control("wolos/x86_64/wolos.db"), "no-cache");
        assert_eq!(cache_control("release.json"), "no-cache");
    }

    #[test]
    fn token_compare() {
        assert!(constant_eq(b"Bearer s", b"Bearer s"));
        assert!(!constant_eq(b"Bearer s", b"Bearer t"));
        assert!(!constant_eq(b"Bearer", b"Bearer s"));
    }
}
