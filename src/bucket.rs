//! Request handling: path rules, auth, cache headers, atomic writes, pull-through from mirrors.
use crate::cache::{Cache, immutable};
use crate::page;
use crate::report::{self, Reports};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use futures_util::TryStreamExt;
use std::io;
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
}

struct Ctx {
    cfg: Config,
    client: reqwest::Client,
    cache: Cache,
    reports: Reports,
}

pub fn app(cfg: Config) -> Router {
    let cache = Cache::open(&cfg.dir, cfg.max_bytes);
    let reports = Reports::open(cfg.reports.clone());
    let client = reqwest::Client::builder()
        .user_agent("wolos-cdn")
        .build()
        .expect("http client");
    Router::new().fallback(handle).with_state(Arc::new(Ctx {
        cfg,
        client,
        cache,
        reports,
    }))
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
    if parts.is_empty() || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

pub fn cache_control(rel: &str) -> &'static str {
    if immutable(rel) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
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

/// POST: keep an error report (anyone). GET `?days=N`: read them (the token).
async fn report(ctx: &Ctx, req: Request) -> Response {
    match *req.method() {
        Method::POST => match axum::body::to_bytes(req.into_body(), report::MAX_BODY).await {
            Ok(b) => ctx.reports.post(&b).into_response(),
            Err(_) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        },
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

async fn handle(State(ctx): State<Arc<Ctx>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    eprintln!("{} {path}", req.method());
    let (cfg, cache) = (&ctx.cfg, &ctx.cache);
    match path.as_str() {
        "/healthz" => return text(StatusCode::OK, "ok\n"),
        "/" => {
            let r = ctx.reports.today();
            return Html(page::html(cache, &cfg.mirrors, &cfg.origin, &r)).into_response();
        }
        "/stats" => {
            let mut v = page::stats(cache, &cfg.mirrors, &cfg.origin);
            v["reports_today"] = serde_json::json!(ctx.reports.today());
            return axum::Json(v).into_response();
        }
        "/report" => return report(&ctx, req).await,
        _ => {}
    }
    let Some(rel) = clean(&path) else {
        return text(StatusCode::BAD_REQUEST, "bad path\n");
    };
    let file = cfg.dir.join(&rel);
    match *req.method() {
        Method::GET | Method::HEAD => get(&ctx, &file, &rel, req).await,
        Method::PUT | Method::DELETE if !authorized(req.headers(), &cfg.token) => {
            text(StatusCode::UNAUTHORIZED, "unauthorized\n")
        }
        Method::PUT => {
            let body = req.into_body().into_data_stream().map_err(io::Error::other);
            match store(StreamReader::new(body), &file).await {
                Ok(n) => {
                    cache.added(&rel, n);
                    StatusCode::CREATED.into_response()
                }
                Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
            }
        }
        Method::DELETE => {
            cache.forget(&rel);
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
        return r;
    }
    let n = match store(StreamReader::new(stream), file).await {
        Ok(n) => n,
        Err(_) => return text(StatusCode::BAD_GATEWAY, "upstream fetch failed\n"),
    };
    for gone in ctx.cache.added(rel, n) {
        eprintln!("evict {gone}");
    }
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
