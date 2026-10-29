//! Error reports from WolOS machines that turned them on: one JSON object per line in
//! `<dir>/YYYY-MM-DD.jsonl`, outside the bucket (never served or evicted with packages). The client
//! address is not kept; the server adds only `at` (unix seconds). Files older than KEEP_DAYS go.
use axum::http::StatusCode;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest report body; a report is a summary, not a log dump.
pub const MAX_BODY: usize = 16 * 1024;
const KEEP_DAYS: u64 = 30;
/// Reports kept per day (at most 16 KiB each, so a day stays under ~320 MiB); past it, 429.
pub const DAY_CAP: u64 = 20_000;

pub struct Reports {
    dir: PathBuf,
    /// Today (days since the epoch) and its counts by kind.
    today: Mutex<(u64, HashMap<String, u64>)>,
}

impl Reports {
    pub fn open(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).expect("reports dir");
        let day = now() / 86400;
        let mut counts = HashMap::new();
        if let Ok(s) = std::fs::read_to_string(dir.join(file(day))) {
            for v in s
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            {
                if let Some(k) = v["kind"].as_str() {
                    *counts.entry(k.to_string()).or_insert(0) += 1;
                }
            }
        }
        Reports {
            dir,
            today: Mutex::new((day, counts)),
        }
    }

    /// Keep one report: 204, or 400 when it is not an object with a valid `kind`.
    pub fn post(&self, body: &[u8]) -> StatusCode {
        let Ok(Value::Object(mut m)) = serde_json::from_slice::<Value>(body) else {
            return StatusCode::BAD_REQUEST;
        };
        let Some(kind) = m
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| valid_kind(k))
        else {
            return StatusCode::BAD_REQUEST;
        };
        let kind = kind.to_string();
        let at = now();
        m.insert("at".into(), at.into());
        let line = Value::Object(Map::from_iter(m)).to_string();
        let mut t = self.today.lock().unwrap();
        let day = at / 86400;
        if t.0 != day {
            *t = (day, HashMap::new());
            self.prune(day);
        }
        if t.1.values().sum::<u64>() >= DAY_CAP {
            return StatusCode::TOO_MANY_REQUESTS;
        }
        let ok = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(file(day)))
            .and_then(|mut f| writeln!(f, "{line}"));
        if ok.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        *t.1.entry(kind).or_insert(0) += 1;
        StatusCode::NO_CONTENT
    }

    /// The last `days` days of reports, oldest first, as JSON lines.
    pub fn read(&self, days: u64) -> String {
        let today = now() / 86400;
        (0..days.clamp(1, KEEP_DAYS))
            .rev()
            .filter_map(|d| std::fs::read_to_string(self.dir.join(file(today - d))).ok())
            .collect()
    }

    pub fn today(&self) -> HashMap<String, u64> {
        self.today.lock().unwrap().1.clone()
    }

    fn prune(&self, today: u64) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let keep: Vec<String> = (0..KEEP_DAYS).map(|d| file(today - d)).collect();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".jsonl") && !keep.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// `unit-failed`, `crash`, `install-failed`: lowercase words and dashes, at most 32.
fn valid_kind(k: &str) -> bool {
    !k.is_empty() && k.len() <= 32 && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `YYYY-MM-DD.jsonl` for a day since the epoch (civil date, UTC).
fn file(day: u64) -> String {
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_file_names() {
        assert_eq!(file(0), "1970-01-01.jsonl");
        assert_eq!(file(20_390), "2025-10-29.jsonl");
        assert_eq!(file(19_782), "2024-02-29.jsonl");
    }

    #[test]
    fn kinds() {
        assert!(valid_kind("unit-failed"));
        assert!(!valid_kind("Unit"));
        assert!(!valid_kind("a b"));
        assert!(!valid_kind(&"a".repeat(33)));
    }
}
