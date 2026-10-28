//! Request handling: path rules, auth, cache headers, atomic writes, pull-through from ORIGIN.
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_util::io::{ReaderStream, StreamReader};

pub struct Config {
    pub dir: PathBuf,
    /// Upload token; empty = read only.
    pub token: String,
    /// Base URL a missing file is fetched from (GitHub release downloads); empty = none.
    pub origin: String,
}

type Ctx = Arc<(Config, reqwest::Client)>;

pub fn app(cfg: Config) -> Router {
    Router::new()
        .fallback(handle)
        .with_state(Arc::new((cfg, reqwest::Client::new())))
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

fn text(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

async fn handle(State(ctx): State<Ctx>, req: Request) -> Response {
    let (cfg, client) = (&ctx.0, &ctx.1);
    let path = req.uri().path().to_string();
    eprintln!("{} {path}", req.method());
    match path.as_str() {
        "/healthz" => return text(StatusCode::OK, "ok\n"),
        "/" => {
            return text(
                StatusCode::OK,
                "WolOS package mirror. pacman: Server = <this host>/$repo-<release>\n",
            );
        }
        _ => {}
    }
    let Some(rel) = clean(&path) else {
        return text(StatusCode::BAD_REQUEST, "bad path\n");
    };
    let file = cfg.dir.join(&rel);
    match *req.method() {
        Method::GET | Method::HEAD => get(&file, &rel, &cfg.origin, client).await,
        Method::PUT | Method::DELETE if !authorized(req.headers(), &cfg.token) => {
            text(StatusCode::UNAUTHORIZED, "unauthorized\n")
        }
        Method::PUT => {
            let body = req.into_body().into_data_stream().map_err(io::Error::other);
            match store(StreamReader::new(body), &file).await {
                Ok(()) => StatusCode::CREATED.into_response(),
                Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
            }
        }
        Method::DELETE => match fs::remove_file(&file).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => StatusCode::NO_CONTENT.into_response(),
            Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n")),
        },
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n"),
    }
}

fn file_response(rel: &str, body: Body) -> Response {
    (
        [
            (header::CACHE_CONTROL, cache_control(rel)),
            (header::CONTENT_TYPE, "application/octet-stream"),
        ],
        body,
    )
        .into_response()
}

async fn open(file: &Path) -> Option<fs::File> {
    let f = fs::File::open(file).await.ok()?;
    f.metadata().await.ok()?.is_file().then_some(f)
}

/// Local file, else the origin's copy: immutable files are stored for the next request.
async fn get(file: &Path, rel: &str, origin: &str, client: &reqwest::Client) -> Response {
    if let Some(f) = open(file).await {
        return file_response(rel, Body::from_stream(ReaderStream::new(f)));
    }
    if origin.is_empty() {
        return text(StatusCode::NOT_FOUND, "not found\n");
    }
    let url = format!("{}/{rel}", origin.trim_end_matches('/'));
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return text(StatusCode::NOT_FOUND, "not found\n"),
    };
    let stream = resp.bytes_stream().map_err(io::Error::other);
    if !cache_control(rel).contains("immutable") {
        return file_response(rel, Body::from_stream(stream));
    }
    if store(StreamReader::new(stream), file).await.is_err() {
        return text(StatusCode::BAD_GATEWAY, "origin fetch failed\n");
    }
    match open(file).await {
        Some(f) => file_response(rel, Body::from_stream(ReaderStream::new(f))),
        None => text(StatusCode::NOT_FOUND, "not found\n"),
    }
}

/// Write `src` to `file` through a temp file in the same directory, then rename.
async fn store(mut src: impl tokio::io::AsyncRead + Unpin, file: &Path) -> io::Result<()> {
    let parent = file.parent().expect("file in bucket");
    fs::create_dir_all(parent).await?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(".upload-{}-{nanos}", std::process::id()));
    let res = async {
        let mut f = fs::File::create(&tmp).await?;
        tokio::io::copy(&mut src, &mut f).await?;
        f.flush().await?;
        f.sync_all().await?;
        fs::rename(&tmp, file).await
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
}
