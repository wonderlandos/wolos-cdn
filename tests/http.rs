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
    // A one-route origin on a plain socket: 200 for the package, 404 for anything else.
    let origin = std::net::TcpListener::bind("127.0.0.1:18440").unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        for mut conn in origin.incoming().flatten() {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut line = String::new();
            let mut rd = BufReader::new(conn.try_clone().unwrap());
            rd.read_line(&mut line).unwrap();
            while let Ok(n) = {
                let mut l = String::new();
                rd.read_line(&mut l)
                    .map(|n| if l == "\r\n" { 0 } else { n })
            } {
                if n == 0 {
                    break;
                }
            }
            let (code, body) = if line.contains(" /wolos-2026-10-20/b-1-1-x86_64.pkg.tar.zst ") {
                ("200 OK", "from-origin")
            } else {
                ("404 Not Found", "nope")
            };
            let _ = write!(
                conn,
                "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let s = start_with("pull", 18433, &[("ORIGIN", "http://127.0.0.1:18440")]);
    let url = format!("{}/wolos-2026-10-20/b-1-1-x86_64.pkg.tar.zst", s.base);
    assert_eq!(
        ureq::get(&url).call().unwrap().into_string().unwrap(),
        "from-origin"
    );
    // Binary files are downloads: the browser saves them instead of showing the bytes.
    let r = ureq::get(&url).call().unwrap();
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(
        r.header("content-disposition"),
        Some("attachment; filename=\"b-1-1-x86_64.pkg.tar.zst\"")
    );
    assert_eq!(r.into_string().unwrap(), "from-origin");
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

/// Past MAX_BYTES the least recently used package goes; ranges resume downloads; `/` is the page.
#[test]
fn budget_range_and_page() {
    let s = start_with("budget", 18434, &[("MAX_BYTES", "10")]);
    let auth = ("Authorization", "Bearer secret");
    for f in ["a.pkg.tar.zst", "b.pkg.tar.zst"] {
        let url = format!("{}/r/{f}", s.base);
        assert_eq!(
            code(ureq::put(&url).set(auth.0, auth.1).send_string("123456")),
            201
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        code(ureq::get(&format!("{}/r/a.pkg.tar.zst", s.base)).call()),
        404
    );
    let r = ureq::get(&format!("{}/r/b.pkg.tar.zst", s.base))
        .set("Range", "bytes=2-")
        .call()
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.into_string().unwrap(), "3456");
    let page = ureq::get(&format!("{}/", s.base))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains("WolOS CDN") && page.contains("r/b.pkg.tar.zst"));
    let stats = ureq::get(&format!("{}/stats", s.base))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(stats.contains("\"max\":10"), "{stats}");
}

/// Error reports: anyone posts a small JSON object (no address kept); only the token reads them.
#[test]
fn reports_are_kept_and_read_with_the_token() {
    let s = start("report", 18450);
    let url = format!("{}/report", s.base);
    let ok = r#"{"kind":"unit-failed","version":"0.11.2","detail":"wold.service"}"#;
    assert_eq!(code(ureq::post(&url).send_string(ok)), 204);
    assert_eq!(code(ureq::post(&url).send_string("not json")), 400);
    assert_eq!(
        code(ureq::post(&url).send_string(r#"{"version":"1"}"#)),
        400,
        "kind required"
    );
    assert_eq!(
        code(ureq::post(&url).send_string(r#"{"kind":"Bad Kind!"}"#)),
        400
    );
    let big = format!(r#"{{"kind":"crash","detail":"{}"}}"#, "x".repeat(20_000));
    assert_eq!(code(ureq::post(&url).send_string(&big)), 413);

    assert_eq!(code(ureq::get(&url).call()), 401);
    let body = ureq::get(&url)
        .set("Authorization", "Bearer secret")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    let lines: Vec<serde_json::Value> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{body}");
    assert_eq!(lines[0]["kind"], "unit-failed");
    assert_eq!(lines[0]["detail"], "wold.service");
    assert!(lines[0]["at"].as_u64().unwrap() > 1_700_000_000);
    assert!(!body.contains("127.0.0.1"), "no client address kept");

    // Not served as a bucket file, counted on /stats.
    assert_eq!(code(ureq::get(&format!("{}/reports", s.base)).call()), 404);
    let stats: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{}/stats", s.base))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stats["reports_today"]["unit-failed"], 1, "{stats}");
    let page = ureq::get(&format!("{}/", s.base))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains("Errors today") && page.contains("unit-failed"));
}

#[path = "../src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

const AUTH: (&str, &str) = ("Authorization", "Bearer secret");

/// Status and JSON body (null when the body is not JSON).
fn answer(r: Result<ureq::Response, ureq::Error>) -> (u16, serde_json::Value) {
    let r = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    };
    let c = r.status();
    let body = r.into_string().unwrap();
    (
        c,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn part(url: &str, a: u64, b: u64, total: u64, body: &[u8]) -> (u16, serde_json::Value) {
    answer(
        ureq::put(url)
            .set(AUTH.0, AUTH.1)
            .set("Content-Range", &format!("bytes {a}-{b}/{total}"))
            .send_bytes(body),
    )
}

fn finish(url: &str, sha: &str) -> (u16, serde_json::Value) {
    answer(
        ureq::post(&format!("{url}?complete=1"))
            .set(AUTH.0, AUTH.1)
            .set("X-Content-Sha256", sha)
            .send_bytes(&[]),
    )
}

/// `n` bytes that differ per seed.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn sessions(s: &Srv) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(s.dir.join("bucket/.uploads"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Parts in any order, a retried part overwrites, overlaps and wrong totals are refused,
/// complete joins in order and verifies; the session and temp files are gone afterwards.
#[test]
fn chunked_upload_out_of_order_with_a_retry() {
    let s = start("chunk", 18460);
    let url = format!("{}/iso/wolos.img", s.base);
    let data = noise(300, 1);
    let (c, v) = part(&url, 200, 299, 300, &data[200..]);
    assert_eq!(c, 202, "{v}");
    assert_eq!(v["received"], 100);
    assert_eq!(v["missing"], serde_json::json!([[0, 199]]));
    // A first try of 0-99 with wrong bytes, then the retry with the right ones.
    assert_eq!(part(&url, 0, 99, 300, &[b'x'; 100]).0, 202);
    assert_eq!(part(&url, 0, 99, 300, &data[..100]).0, 202);
    let (c, v) = finish(&url, &sha256::hex(&data));
    assert_eq!(c, 409, "parts missing: {v}");
    assert_eq!(v["missing"], serde_json::json!([[100, 199]]));
    assert_eq!(part(&url, 50, 149, 300, &data[50..150]).0, 409, "overlap");
    assert_eq!(part(&url, 100, 199, 301, &data[100..200]).0, 409, "total");
    assert_eq!(
        part(&url, 100, 199, 300, &data[100..150]).0,
        400,
        "short body"
    );
    assert_eq!(
        code(ureq::get(&url).call()),
        404,
        "not in place before complete"
    );
    let (c, v) = part(&url, 100, 199, 300, &data[100..200]);
    assert_eq!((c, &v["received"]), (202, &serde_json::json!(300)));
    let (c, v) = finish(&url, &sha256::hex(&data).to_uppercase());
    assert_eq!(c, 201, "{v}");
    assert_eq!(v["size"], 300);
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut ureq::get(&url).call().unwrap().into_reader(), &mut got)
        .unwrap();
    assert_eq!(got, data);
    assert!(sessions(&s).is_empty(), "session left behind");
    let left: Vec<_> = std::fs::read_dir(s.dir.join("bucket/iso"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != "wolos.img")
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // Sessions are never served or listed as bucket files.
    assert_eq!(
        code(ureq::get(&format!("{}/.uploads/x", s.base)).call()),
        400
    );
    // The plain PUT still works for small files.
    assert_eq!(
        code(ureq::put(&url).set(AUTH.0, AUTH.1).send_string("small")),
        201
    );
    assert_eq!(
        ureq::get(&url).call().unwrap().into_string().unwrap(),
        "small"
    );
}

#[test]
fn wrong_digest_is_refused() {
    let s = start("digest", 18461);
    let url = format!("{}/iso/wolos.iso", s.base);
    let data = noise(64, 2);
    assert_eq!(part(&url, 0, 31, 64, &data[..32]).0, 202);
    assert_eq!(part(&url, 32, 63, 64, &data[32..]).0, 202);
    assert_eq!(finish(&url, "abc").0, 400, "not a digest");
    assert_eq!(
        code(
            ureq::post(&format!("{url}?complete=1"))
                .set(AUTH.0, AUTH.1)
                .send_bytes(&[])
        ),
        400,
        "no digest"
    );
    let (c, v) = finish(&url, &"0".repeat(64));
    assert_eq!(c, 422, "{v}");
    assert_eq!(v["got"], sha256::hex(&data));
    assert_eq!(code(ureq::get(&url).call()), 404, "nothing put in place");
    assert_eq!(finish(&url, &sha256::hex(&data)).0, 404, "session dropped");
    assert!(sessions(&s).is_empty());
    let left: Vec<_> = std::fs::read_dir(s.dir.join("bucket/iso"))
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "temp file left: {left:?}");
}

#[test]
fn part_limits_and_auth() {
    let s = start_with("limits", 18462, &[("MAX_BYTES", "1000")]);
    let url = format!("{}/iso/big.iso", s.base);
    let mib = 1024 * 1024;
    let (c, v) = part(&url, 0, 100 * mib, 1000, b"x");
    assert_eq!(c, 416, "outside the total: {v}");
    // Over 100 MiB in one part: refused before the body is read.
    let (c, v) = part(&url, 0, 100 * mib, 200 * mib, b"x");
    assert_eq!(c, 413, "{v}");
    assert!(v["error"].as_str().unwrap().contains("part"), "{v}");
    assert_eq!(
        part(&url, 0, 9, 2000, &[0; 10]).0,
        413,
        "total over MAX_BYTES"
    );
    assert_eq!(part(&url, 9, 0, 1000, &[0; 10]).0, 416);
    let (c, _) = answer(
        ureq::put(&url)
            .set(AUTH.0, AUTH.1)
            .set("Content-Range", "bytes 0-9/*")
            .send_bytes(&[0; 10]),
    );
    assert_eq!(c, 400);
    assert_eq!(
        code(
            ureq::put(&url)
                .set("Content-Range", "bytes 0-9/10")
                .send_bytes(&[0; 10])
        ),
        401
    );
    assert_eq!(
        code(
            ureq::post(&format!("{url}?complete=1"))
                .set("X-Content-Sha256", &"0".repeat(64))
                .send_bytes(&[])
        ),
        401
    );
    assert_eq!(code(ureq::delete(&format!("{url}?upload=1")).call()), 401);
    assert_eq!(code(ureq::post(&url).send_bytes(&[])), 405);
    assert!(sessions(&s).is_empty(), "refused parts opened a session");
}

#[test]
fn abort_drops_the_session() {
    let s = start("abort", 18463);
    let url = format!("{}/iso/wolos.iso", s.base);
    assert_eq!(part(&url, 0, 9, 20, &[1; 10]).0, 202);
    assert_eq!(sessions(&s).len(), 1);
    let abort = || {
        answer(
            ureq::delete(&format!("{url}?upload=1"))
                .set(AUTH.0, AUTH.1)
                .call(),
        )
    };
    let (c, v) = abort();
    assert_eq!((c, &v["aborted"]), (200, &serde_json::json!(true)));
    assert!(sessions(&s).is_empty());
    assert_eq!(finish(&url, &"0".repeat(64)).0, 404);
    assert_eq!(abort().1["aborted"], false);
    // A new session for the same path may use another total.
    assert_eq!(part(&url, 0, 4, 5, &[2; 5]).0, 202);
}

#[test]
fn stale_sessions_are_swept() {
    let s = start("sweep", 18464);
    assert_eq!(
        part(&format!("{}/a.iso", s.base), 0, 9, 20, &[1; 10]).0,
        202
    );
    let old = sessions(&s).pop().unwrap();
    let mf = old.join("manifest.json");
    let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&mf).unwrap()).unwrap();
    assert_eq!(m["path"], "a.iso");
    assert_eq!(m["total"], 20);
    assert_eq!(m["ranges"], serde_json::json!([[0, 9]]));
    m["started"] = serde_json::json!(m["started"].as_u64().unwrap() - 25 * 3600);
    std::fs::write(&mf, m.to_string()).unwrap();
    // A new session sweeps the stale ones.
    assert_eq!(
        part(&format!("{}/b.iso", s.base), 0, 9, 20, &[1; 10]).0,
        202
    );
    assert!(!old.exists(), "stale session kept");
    assert_eq!(sessions(&s).len(), 1);
}

/// KEEP_ISOS=2: a third ISO in a directory removes the oldest with its checksum and signature;
/// other directories are left alone.
#[test]
fn keep_latest_isos() {
    let s = start_with("keep", 18465, &[("KEEP_ISOS", "2")]);
    let put = |p: &str, body: &str| {
        let url = format!("{}/{p}", s.base);
        assert_eq!(
            code(ureq::put(&url).set(AUTH.0, AUTH.1).send_string(body)),
            201
        );
    };
    put("other/x.iso", "x");
    for v in ["1", "2"] {
        put(&format!("iso/wolos-{v}.iso"), v);
        put(&format!("iso/wolos-{v}.iso.sha256"), v);
        put(&format!("iso/wolos-{v}.iso.sig"), v);
        std::thread::sleep(Duration::from_millis(20));
    }
    let url = format!("{}/iso/wolos-3.iso", s.base);
    let data = noise(10, 3);
    assert_eq!(part(&url, 5, 9, 10, &data[5..]).0, 202);
    assert_eq!(part(&url, 0, 4, 10, &data[..5]).0, 202);
    let (c, v) = finish(&url, &sha256::hex(&data));
    assert_eq!(c, 201, "{v}");
    let mut removed: Vec<_> = v["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
    removed.sort();
    assert_eq!(
        removed,
        [
            "iso/wolos-1.iso",
            "iso/wolos-1.iso.sha256",
            "iso/wolos-1.iso.sig"
        ]
    );
    let b = s.dir.join("bucket");
    assert!(!b.join("iso/wolos-1.iso").exists() && !b.join("iso/wolos-1.iso.sig").exists());
    for f in [
        "iso/wolos-2.iso",
        "iso/wolos-2.iso.sig",
        "iso/wolos-3.iso",
        "other/x.iso",
    ] {
        assert!(b.join(f).exists(), "{f}");
    }
}

/// Over the budget, packages go but the newest ISO (the current release) stays.
#[test]
fn newest_iso_survives_eviction() {
    let s = start_with("newest", 18466, &[("MAX_BYTES", "10")]);
    for f in ["iso/wolos.iso", "r/a.pkg.tar.zst", "r/b.pkg.tar.zst"] {
        let url = format!("{}/{f}", s.base);
        assert_eq!(
            code(ureq::put(&url).set(AUTH.0, AUTH.1).send_string("123456")),
            201
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        code(ureq::get(&format!("{}/r/a.pkg.tar.zst", s.base)).call()),
        404
    );
    assert_eq!(
        ureq::get(&format!("{}/iso/wolos.iso", s.base))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
        "123456"
    );
}

/// The client: a few MiB in 1 MiB parts, 4 at a time, then complete with the digest.
#[test]
fn cdn_put_client_uploads_in_parts() {
    if Command::new("curl").arg("--version").output().is_err() {
        eprintln!("curl missing: skipped");
        return;
    }
    let s = start("client", 18467);
    let data = noise(3 * 1024 * 1024 + 12345, 4);
    let local = s.dir.join("local.iso");
    std::fs::write(&local, &data).unwrap();
    let out = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/cdn-put"))
        .arg(&local)
        .arg("/iso/up.iso")
        .args(["--base", &s.base])
        .env("UPLOAD_TOKEN", "secret")
        .env("CDN_PUT_PART_MIB", "1")
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{log}");
    assert!(log.contains("4 part(s)"), "{log}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["sha256"], sha256::hex(&data));
    assert_eq!(
        std::fs::read(s.dir.join("bucket/iso/up.iso")).unwrap(),
        data
    );
    // A wrong token fails cleanly.
    let out = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/cdn-put"))
        .arg(&local)
        .arg("iso/no.iso")
        .args(["--base", &s.base])
        .env("UPLOAD_TOKEN", "wrong")
        .env("CDN_PUT_PART_MIB", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!s.dir.join("bucket/iso/no.iso").exists());
}
