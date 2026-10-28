//! wolos-cdn: the WolOS package bucket. GET serves files (pacman repos, ISOs, release manifests);
//! PUT and DELETE with the upload token change them. Writes are atomic (temp file + rename), so a
//! pacman client never sees half a database.
//!
//! Env: HOST (0.0.0.0) and PORT (80) as the hosting runtime sets them, DATA_DIR (./data),
//! UPLOAD_TOKEN (required for writes; unset = read only), ORIGIN (base URL missing files are pulled
//! from and kept; unset = none).

mod bucket;

#[tokio::main]
async fn main() {
    let dir = std::path::PathBuf::from(env("DATA_DIR", "data"));
    std::fs::create_dir_all(&dir).expect("DATA_DIR");
    let addr = format!("{}:{}", env("HOST", "0.0.0.0"), env("PORT", "80"));
    let cfg = bucket::Config {
        dir: dir.clone(),
        token: std::env::var("UPLOAD_TOKEN").unwrap_or_default(),
        origin: std::env::var("ORIGIN").unwrap_or_default(),
    };
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("listen");
    eprintln!("wolos-cdn: serving {} on {addr}", dir.display());
    axum::serve(listener, bucket::app(cfg))
        .await
        .expect("serve");
}

fn env(k: &str, default: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}
