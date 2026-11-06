//! Request handling: path rules, auth, cache headers, atomic writes, chunked uploads,
//! pull-through from mirrors.
use crate::cache::{Cache, immutable};
use crate::page;
use crate::releases::Releases;
use crate::report::{self, Reports};
use crate::upload::{self, Uploads};
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use futures_util::TryStreamExt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_util::io::StreamReader;
use tower::ServiceExt;
use tower_http::services::ServeFile;

pub struct Config {
    pub dir: PathBuf,
    /// Upload token; empty = read only.
    pub token: String,
    /// Base URL a missing file is fetched from (GitHub release downloads); empty = none.
    pub origin: String,
    /// Upstream mirrors by first path segment: `/archive/x` is fetched from `<url>/x`.
    pub mirrors: Vec<(String, String)>,
    /// Disk budget in bytes for the bucket.
    pub max_bytes: u64,
    /// Where error reports are kept (outside `dir`).
    pub reports: PathBuf,
    /// ISOs kept per directory after an upload (newest first); 0 keeps all.
    pub keep_isos: usize,
}

struct Ctx {
    cfg: Config,
    client: reqwest::Client,
    cache: Cache,
    reports: Reports,
    uploads: Uploads,
    releases: Releases,
}

pub fn app(cfg: Config) -> Router {
    let cache = Cache::open(&cfg.dir, cfg.max_bytes)
        .with_mirrors(cfg.mirrors.iter().map(|(p, _)| p.clone()).collect());
    let reports = Reports::open(cfg.reports.clone());
    let uploads = Uploads::new(&cfg.dir, cfg.max_bytes);
    uploads.sweep(true);
    let releases = Releases::new(&cfg.dir);
    let client = reqwest::Client::builder()
        .user_agent("wolos-cdn")
        .build()
        .expect("http client");
    let ctx = Arc::new(Ctx {
        cfg,
        client,
        cache,
        reports,
        uploads,
        releases,
    });
    let weak = Arc::downgrade(&ctx);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
            let Some(ctx) = weak.upgrade() else { return };
            ctx.uploads.sweep(false);
        }
    });
    Router::new()
        .fallback(handle)
        .layer(axum::middleware::map_response(hardened))
        .with_state(ctx)
}

/// On every answer (QA-WEB-PRIVACY-5): files are never sniffed into other types, and the pages (which
/// have no script, form or image) get a policy that allows none. Every Cache-Control carries
/// `no-transform`: the files are content-addressed or signed, so a proxy must never change their
/// bytes (Square Cloud's Cloudflare ran Polish on `plugins/pictures/<sha256>.png` and the Hub build
/// dropped every icon as a hash mismatch; its email obfuscation turned `ort@1.x` file names into
/// "[email protected]" on the pages).
async fn hardened(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if h.get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/html"))
    {
        h.insert("x-frame-options", HeaderValue::from_static("DENY"));
        h.insert(
            "content-security-policy",
            HeaderValue::from_static(page::CSP),
        );
        if !h.contains_key(header::CACHE_CONTROL) {
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
    }
    if let Some(cc) = h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        if !cc.split(',').any(|d| d.trim() == "no-transform") {
            if let Ok(v) = HeaderValue::from_str(&format!("{cc}, no-transform")) {
                h.insert(header::CACHE_CONTROL, v);
            }
        }
    }
    res
}

/// Who is posting, for the day's limit only (never stored): Cloudflare's header, else the last
/// `X-Forwarded-For` entry (the one the nearest proxy added), else the connection's own address.
fn client(req: &Request) -> String {
    let head = |n: &str| {
        req.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if let Some(ip) = head("cf-connecting-ip").filter(|v| !v.is_empty()) {
        return ip.to_string();
    }
    if let Some(ip) = head("x-forwarded-for")
        .and_then(|v| v.rsplit(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return ip.to_string();
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(String::new, |c| c.0.ip().to_string())
}

/// `name=url,name=url` (MIRRORS env) as prefix pairs.
pub fn mirrors(spec: &str) -> Vec<(String, String)> {
    spec.split(',')
        .filter_map(|m| m.split_once('='))
        .map(|(p, u)| {
            (
                p.trim().to_string(),
                u.trim().trim_end_matches('/').to_string(),
            )
        })
        .filter(|(p, u)| !p.is_empty() && !u.is_empty())
        .collect()
}

/// Where a missing file comes from: the mirror its first segment names, else the origin.
pub fn upstream(rel: &str, mirrors: &[(String, String)], origin: &str) -> Option<String> {
    if let Some((first, rest)) = rel.split_once('/')
        && let Some((_, url)) = mirrors.iter().find(|(p, _)| p == first)
    {
        return Some(format!("{url}/{rest}"));
    }
    (!origin.is_empty()).then(|| format!("{}/{rel}", origin.trim_end_matches('/')))
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
    // `.upload*` names are the server's own: temp files and upload sessions.
    if parts.is_empty() || parts.iter().any(|s| *s == ".." || s.starts_with(".upload")) {
        return None;
    }
    Some(parts.join("/"))
}

/// The set is bounded only by the signed index and the publisher; GC against the index is a later task.
/// A Hub listing picture, `plugins/pictures/<sha256>.<png|webp>` (hub-pipeline publish.sh): named by
/// its hash, so it never changes and browsers may keep it. Not [`immutable`] for the disk budget:
/// it is uploaded here and has no upstream copy, so it is never evicted.
pub fn hub_picture(rel: &str) -> bool {
    rel.strip_prefix("plugins/pictures/")
        .and_then(|f| f.strip_suffix(".png").or_else(|| f.strip_suffix(".webp")))
        .is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// `no-transform` on both: the bytes are content-addressed or signed, see [`hardened`].
pub fn cache_control(rel: &str) -> &'static str {
    if immutable(rel) || hub_picture(rel) {
        "public, max-age=31536000, immutable, no-transform"
    } else {
        "no-cache, no-transform"
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let want = format!("Bearer {token}");
    !token.is_empty()
        && headers
            .get(header::AUTHORIZATION)
            .is_some_and(|v| constant_eq(v.as_bytes(), want.as_bytes()))
}

/// POST: keep an error report (anyone, with a limit per address). GET `?days=N`: read them (the token).
async fn report(ctx: &Ctx, req: Request) -> Response {
    match *req.method() {
        Method::POST => {
            let from = client(&req);
            match axum::body::to_bytes(req.into_body(), report::MAX_BODY).await {
                Ok(b) => ctx.reports.post(&b, &from).into_response(),
                Err(_) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            }
        }
        Method::GET if authorized(req.headers(), &ctx.cfg.token) => {
            let days = req
                .uri()
                .query()
                .and_then(|q| q.strip_prefix("days="))
                .and_then(|d| d.parse().ok())
                .unwrap_or(1);
            (
                [(header::CONTENT_TYPE, "application/x-ndjson")],
                ctx.reports.read(days),
            )
                .into_response()
        }
        Method::GET => text(StatusCode::UNAUTHORIZED, "unauthorized\n"),
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn text(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

fn json(code: StatusCode, v: serde_json::Value) -> Response {
    (code, axum::Json(v)).into_response()
}

fn reply(r: Result<serde_json::Value, upload::Fail>, ok: StatusCode) -> Response {
    match r {
        Ok(v) => json(ok, v),
        Err((code, v)) => json(code, v),
    }
}

/// A stored file is in place: ISOs past KEEP_ISOS go, then the disk budget is enforced.
fn stored(ctx: &Ctx, rel: &str, n: u64) -> Vec<String> {
    let removed = upload::keep_latest(&ctx.cfg.dir, rel, ctx.cfg.keep_isos);
    for r in &removed {
        ctx.cache.forget(r);
        ctx.releases.changed(r);
    }
    added(ctx, rel, n);
    removed
}

/// Record a new file in the budget (evicting past it); the releases index follows both.
fn added(ctx: &Ctx, rel: &str, n: u64) {
    ctx.releases.changed(rel);
    for gone in ctx.cache.added(rel, n) {
        eprintln!("evict {gone}");
        ctx.releases.changed(&gone);
    }
}

/// `/releases.json`, `/releases/latest.json`: read by the site and by scripts on any origin.
fn index_json(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response()
}

/// The release routes (GET and HEAD only); None for any other path.
fn releases(ctx: &Ctx, path: &str, method: &Method) -> Option<Response> {
    if !matches!(
        path,
        "/releases.json" | "/releases/latest.json" | "/releases" | "/releases/"
    ) {
        return None;
    }
    if !matches!(*method, Method::GET | Method::HEAD) {
        return Some(text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n"));
    }
    let idx = ctx.releases.get();
    Some(match path {
        "/releases.json" => index_json(idx.json.clone()),
        "/releases/latest.json" => match idx.list.first() {
            Some(r) => index_json(r.json().to_string()),
            None => json(
                StatusCode::NOT_FOUND,
                serde_json::json!({"error": "no releases"}),
            ),
        },
        _ => (
            [(header::CACHE_CONTROL, "no-cache")],
            Html(page::releases(&idx.list)),
        )
            .into_response(),
    })
}

/// `PUT` with `Content-Range`: one part of a chunked upload.
async fn put_part(ctx: &Ctx, rel: &str, req: Request) -> Response {
    let range = req
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(upload::content_range);
    let Some(range) = range else {
        return json(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "Content-Range must be `bytes a-b/total`"}),
        );
    };
    let len = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok());
    let body = req.into_body().into_data_stream().map_err(io::Error::other);
    let r = ctx
        .uploads
        .part(rel, range, len, StreamReader::new(body))
        .await;
    reply(r, StatusCode::ACCEPTED)
}

/// `POST ?complete=1`: join the parts, check `X-Content-Sha256`, put the file in place.
async fn complete(ctx: Arc<Ctx>, rel: String, file: PathBuf, headers: &HeaderMap) -> Response {
    let want = headers
        .get("x-content-sha256")
        .and_then(|v| v.to_str().ok())
        .and_then(upload::digest);
    let Some(want) = want else {
        return json(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "X-Content-Sha256 must be 64 hex digits"}),
        );
    };
    let m = match ctx.uploads.begin_complete(&rel) {
        Ok(m) => m,
        Err((code, v)) => return json(code, v),
    };
    // Off the async threads, and run to the end even if the client goes away meanwhile.
    let job = tokio::task::spawn_blocking(move || {
        let n = ctx.uploads.assemble(&m, &file, &want)?;
        let removed = stored(&ctx, &rel, n);
        eprintln!("upload {rel}: complete, {n} bytes, sha256 {want}");
        Ok(serde_json::json!({"path": rel, "size": n, "sha256": want, "removed": removed}))
    });
    match job.await {
        Ok(r) => reply(r, StatusCode::CREATED),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
    }
}

async fn handle(State(ctx): State<Arc<Ctx>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    eprintln!("{} {path}", req.method());
    let (cfg, cache) = (&ctx.cfg, &ctx.cache);
    match path.as_str() {
        "/healthz" => return text(StatusCode::OK, "ok\n"),
        // The front page is for people downloading: releases and the pacman line, nothing about the
        // operation (QA-WEB-PRIVACY-10).
        "/" => return Html(page::home()).into_response(),
        "/robots.txt" => {
            return (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                page::ROBOTS,
            )
                .into_response();
        }
        // The operator's view (storage, hit rate, last files served, report counts): the token only,
        // never indexed, never reloading by itself.
        "/status" | "/stats" if !authorized(req.headers(), &cfg.token) => {
            return text(StatusCode::UNAUTHORIZED, "unauthorized\n");
        }
        "/status" => {
            let r = ctx.reports.today();
            return (
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (
                        header::HeaderName::from_static("x-robots-tag"),
                        "noindex, nofollow",
                    ),
                ],
                Html(page::html(cache, &cfg.mirrors, &cfg.origin, &r)),
            )
                .into_response();
        }
        "/stats" => {
            let mut v = page::stats(cache, &cfg.mirrors, &cfg.origin);
            v["reports_today"] = serde_json::json!(ctx.reports.today());
            return ([(header::CACHE_CONTROL, "no-store")], axum::Json(v)).into_response();
        }
        "/report" => return report(&ctx, req).await,
        _ => {}
    }
    if let Some(r) = releases(&ctx, &path, req.method()) {
        return r;
    }
    let Some(rel) = clean(&path) else {
        return text(StatusCode::BAD_REQUEST, "bad path\n");
    };
    let file = cfg.dir.join(&rel);
    let query = req.uri().query().unwrap_or("").to_string();
    let flag = |f: &str| query.split('&').any(|p| p == f);
    match *req.method() {
        Method::GET | Method::HEAD => get(&ctx, &file, &rel, req).await,
        Method::POST if !flag("complete=1") => {
            text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n")
        }
        Method::PUT | Method::DELETE | Method::POST if !authorized(req.headers(), &cfg.token) => {
            text(StatusCode::UNAUTHORIZED, "unauthorized\n")
        }
        Method::PUT if req.headers().contains_key(header::CONTENT_RANGE) => {
            put_part(&ctx, &rel, req).await
        }
        Method::PUT => {
            let body = req.into_body().into_data_stream().map_err(io::Error::other);
            match store(StreamReader::new(body), &file).await {
                Ok(n) => {
                    stored(&ctx, &rel, n);
                    StatusCode::CREATED.into_response()
                }
                Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
            }
        }
        Method::POST => complete(ctx.clone(), rel, file, req.headers()).await,
        Method::DELETE if flag("upload=1") => match ctx.uploads.abort(&rel) {
            Ok(had) => json(
                StatusCode::OK,
                serde_json::json!({"path": rel, "aborted": had}),
            ),
            Err((code, v)) => json(code, v),
        },
        Method::DELETE => {
            cache.forget(&rel);
            ctx.releases.changed(&rel);
            match fs::remove_file(&file).await {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
            }
        }
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n"),
    }
}

/// Binary files (packages, ISOs, archives, signatures, databases) are downloads: the browser
/// saves them instead of showing the bytes. Text files (checksums, JSON, pages) keep their type.
pub fn is_download(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    [
        ".zst", ".iso", ".img", ".xz", ".gz", ".tar", ".sig", ".db", ".files", ".efi", ".asc",
    ]
    .iter()
    .any(|ext| name.ends_with(ext))
}

fn download_headers(rel: &str, r: &mut Response) {
    if !is_download(rel) {
        return;
    }
    let name = rel.rsplit('/').next().unwrap_or(rel).replace('"', "");
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
}

/// The local file (Range and HEAD included, so ISO downloads resume).
async fn serve(rel: &str, file: &Path, req: Request) -> Response {
    let mut r = match ServeFile::new(file).oneshot(req).await {
        Ok(r) => r.map(Body::new),
        Err(e) => match e {},
    };
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(rel)),
    );
    download_headers(rel, &mut r);
    r
}

/// Local file, else the upstream copy: immutable files are stored for the next request.
async fn get(ctx: &Ctx, file: &Path, rel: &str, req: Request) -> Response {
    if fs::metadata(file).await.is_ok_and(|m| m.is_file()) {
        ctx.cache.touch(rel);
        let r = serve(rel, file, req).await;
        ctx.cache.hit(len(&r), false);
        return r;
    }
    let Some(url) = upstream(rel, &ctx.cfg.mirrors, &ctx.cfg.origin) else {
        return text(StatusCode::NOT_FOUND, "not found\n");
    };
    let resp = match ctx.client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return text(StatusCode::NOT_FOUND, "not found\n"),
    };
    let stream = resp.bytes_stream().map_err(io::Error::other);
    if !immutable(rel) {
        ctx.cache.hit(0, true);
        let mut r = Body::from_stream(stream).into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        download_headers(rel, &mut r);
        return r;
    }
    let n = match store(StreamReader::new(stream), file).await {
        Ok(n) => n,
        Err(_) => return text(StatusCode::BAD_GATEWAY, "upstream fetch failed\n"),
    };
    added(ctx, rel, n);
    ctx.cache.hit(n, true);
    serve(rel, file, req).await
}

fn len(r: &Response) -> u64 {
    r.headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .unwrap_or(0)
}

/// Write `src` to `file` through a temp file in the same directory, then rename. Returns the size.
async fn store(mut src: impl tokio::io::AsyncRead + Unpin, file: &Path) -> io::Result<u64> {
    let parent = file.parent().expect("file in bucket");
    fs::create_dir_all(parent).await?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(".upload-{}-{nanos}", std::process::id()));
    let res = async {
        let mut f = fs::File::create(&tmp).await?;
        let n = tokio::io::copy(&mut src, &mut f).await?;
        f.flush().await?;
        f.sync_all().await?;
        fs::rename(&tmp, file).await?;
        Ok(n)
    }
    .await;
    if res.is_err() {
        let _ = fs::remove_file(&tmp).await;
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
        for bad in [
            "/../x",
            "/a/../../x",
            "/%2e%2e/x",
            "/",
            "/a/",
            "/a\\b",
            "",
            "/.uploads/x/manifest.json",
            "/a/.upload-1-2",
        ] {
            assert_eq!(clean(bad), None, "{bad}");
        }
    }

    #[test]
    fn packages_cache_forever_databases_never() {
        assert!(cache_control("wolos/x86_64/a-1-1-x86_64.pkg.tar.zst").contains("immutable"));
        assert!(cache_control("iso/wolos-2026.11.iso").contains("immutable"));
        assert_eq!(cache_control("wolos/x86_64/wolos.db"), "no-cache, no-transform");
        assert_eq!(cache_control("release.json"), "no-cache, no-transform");
    }

    #[test]
    fn hub_pictures_cache_forever_and_are_never_evicted() {
        let pic =
            "plugins/pictures/95d648d3d175b7df65af3f36caacaacc448897768f22945b5637685a8d5242f6";
        for ext in ["png", "webp"] {
            let rel = format!("{pic}.{ext}");
            assert_eq!(
                cache_control(&rel),
                "public, max-age=31536000, immutable, no-transform"
            );
            // Uploaded here only (no upstream copy): the disk budget must never drop one.
            assert!(!immutable(&rel), "{rel}");
        }
        for other in [
            "plugins/index.json",
            "plugins/pictures/x.png",
            "plugins/pictures/95d648d3d175b7df65af3f36caacaacc448897768f22945b5637685a8d5242f6.gif",
            "plugins/pictures/95D648D3D175B7DF65AF3F36CAACAACC448897768F22945B5637685A8D5242F6.png",
            "plugins/pictures/sub/95d648d3d175b7df65af3f36caacaacc448897768f22945b5637685a8d5242f6.png",
            "other/pictures/95d648d3d175b7df65af3f36caacaacc448897768f22945b5637685a8d5242f6.png",
        ] {
            assert_eq!(cache_control(other), "no-cache, no-transform", "{other}");
        }
    }

    #[test]
    fn token_compare() {
        assert!(constant_eq(b"Bearer s", b"Bearer s"));
        assert!(!constant_eq(b"Bearer s", b"Bearer t"));
        assert!(!constant_eq(b"Bearer", b"Bearer s"));
    }

    #[test]
    fn mirrors_by_first_segment_rest_from_the_origin() {
        let m = mirrors(
            "archive=https://archive.archlinux.org/, cachy=https://mirror.cachyos.org/repo,bad",
        );
        assert_eq!(m.len(), 2);
        assert_eq!(
            upstream(
                "archive/repos/2026/10/20/core/os/x86_64/a.pkg.tar.zst",
                &m,
                "https://o"
            )
            .as_deref(),
            Some("https://archive.archlinux.org/repos/2026/10/20/core/os/x86_64/a.pkg.tar.zst")
        );
        assert_eq!(
            upstream("wolos-2026-10-20/b.pkg.tar.zst", &m, "https://o/").as_deref(),
            Some("https://o/wolos-2026-10-20/b.pkg.tar.zst")
        );
        assert_eq!(upstream("x/y", &m, ""), None);
    }
}
