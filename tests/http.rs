//! End to end over HTTP against the real binary.
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Srv {
    child: Child,
    base: String,
    dir: std::path::PathBuf,
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(name: &str, port: u16) -> Srv {
    start_with(name, port, &[])
}

fn start_with(name: &str, port: u16, env: &[(&str, &str)]) -> Srv {
    let dir = std::env::temp_dir().join(format!("wolos-cdn-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let child = Command::new(env!("CARGO_BIN_EXE_wolos-cdn"))
        .env("PORT", port.to_string())
        .env("DATA_DIR", dir.join("bucket"))
        .env("UPLOAD_TOKEN", "secret")
        .envs(env.iter().copied())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let t = Instant::now();
    while ureq::get(&format!("{base}/healthz")).call().is_err() {
        assert!(t.elapsed() < Duration::from_secs(5), "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Srv { child, base, dir }
}

fn code(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn upload_needs_the_token() {
    let s = start("auth", 18431);
    let url = format!("{}/wolos/x86_64/a.pkg.tar.zst", s.base);
    assert_eq!(code(ureq::put(&url).send_string("x")), 401);
    assert_eq!(
        code(
            ureq::put(&url)
                .set("Authorization", "Bearer wrong")
                .send_string("x")
        ),
        401
    );
    assert!(
        !s.dir.join("bucket/wolos").exists(),
        "refused upload wrote a file"
    );
}

#[test]
fn upload_serve_delete() {
    let s = start("rw", 18432);
    let pkg = format!("{}/wolos/x86_64/a-1-1-x86_64.pkg.tar.zst", s.base);
    let auth = ("Authorization", "Bearer secret");
    assert_eq!(
        code(ureq::put(&pkg).set(auth.0, auth.1).send_string("pkg")),
        201
    );
    let r = ureq::get(&pkg).call().unwrap();
    assert!(r.header("Cache-Control").unwrap().contains("immutable"));
    assert_eq!(r.into_string().unwrap(), "pkg");
    assert_eq!(
        code(
            ureq::put(&format!("{}/wolos/x86_64/wolos.db", s.base))
                .set(auth.0, auth.1)
                .send_string("db")
        ),
        201
    );
    let r = ureq::get(&format!("{}/wolos/x86_64/wolos.db", s.base))
        .call()
        .unwrap();
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    // No listings, no anonymous deletes.
    assert_eq!(code(ureq::get(&format!("{}/wolos/", s.base)).call()), 400);
    assert_eq!(code(ureq::delete(&pkg).call()), 401);
    assert_eq!(code(ureq::delete(&pkg).set(auth.0, auth.1).call()), 204);
    assert_eq!(code(ureq::get(&pkg).call()), 404);
    // Atomic writes leave no temp files behind.
    let left: Vec<_> = std::fs::read_dir(s.dir.join("bucket/wolos/x86_64"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".upload"))
        .collect();
    assert!(left.is_empty());
}

/// A missing file is fetched once from ORIGIN (GitHub release assets), stored, then served locally.
#[test]
fn pulls_through_from_the_origin() {
    let origin = tiny_http::Server::http("127.0.0.1:18440").unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for req in origin.incoming_requests() {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let r = if req.url() == "/wolos-2026-10-20/b-1-1-x86_64.pkg.tar.zst" {
                tiny_http::Response::from_string("from-origin").with_status_code(200)
            } else {
                tiny_http::Response::from_string("nope").with_status_code(404)
            };
            let _ = req.respond(r);
        }
    });
    let s = start_with("pull", 18433, &[("ORIGIN", "http://127.0.0.1:18440")]);
    let url = format!("{}/wolos-2026-10-20/b-1-1-x86_64.pkg.tar.zst", s.base);
    assert_eq!(
        ureq::get(&url).call().unwrap().into_string().unwrap(),
        "from-origin"
    );
    assert_eq!(
        ureq::get(&url).call().unwrap().into_string().unwrap(),
        "from-origin"
    );
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "second GET served from disk"
    );
    assert!(
        s.dir
            .join("bucket/wolos-2026-10-20/b-1-1-x86_64.pkg.tar.zst")
            .exists()
    );
    // Databases change on every publish: always fresh from the origin, never stored.
    assert_eq!(
        code(ureq::get(&format!("{}/wolos-2026-10-20/missing.db", s.base)).call()),
        404
    );
    assert!(!s.dir.join("bucket/wolos-2026-10-20/missing.db").exists());
}
