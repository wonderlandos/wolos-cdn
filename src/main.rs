//! wolos-cdn: the WolOS package bucket. GET serves files (pacman repos, ISOs, release manifests);
//! PUT and DELETE with the upload token change them. Writes are atomic (temp file + rename), so a
//! pacman client never sees half a database.
//!
//! Env: PORT (80), DATA_DIR (./data), UPLOAD_TOKEN (required for writes; unset = read only),
//! WORKERS (threads, 8), ORIGIN (base URL missing files are pulled from and kept; unset = none).

mod bucket;

fn main() {
    let dir = std::path::PathBuf::from(env("DATA_DIR", "data"));
    std::fs::create_dir_all(&dir).expect("DATA_DIR");
    let addr = format!("0.0.0.0:{}", env("PORT", "80"));
    let cfg = bucket::Config {
        dir: dir.clone(),
        token: std::env::var("UPLOAD_TOKEN").unwrap_or_default(),
        origin: std::env::var("ORIGIN").unwrap_or_default(),
    };
    let workers = env("WORKERS", "8").parse().unwrap_or(8);
    eprintln!("wolos-cdn: serving {} on {addr}", dir.display());
    bucket::serve(&addr, cfg, workers);
}

fn env(k: &str, default: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}
