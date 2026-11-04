//! Error reports from WolOS machines that turned them on: one JSON object per line in
//! `<dir>/YYYY-MM-DD.jsonl`, outside the bucket (never served or evicted with packages). The client
//! address is never written down: it is only counted, as a keyed hash in memory, for the day's
//! per-address limit (QA-WEB-PRIVACY-6), and the count is gone at midnight UTC or on a restart.
//! A stored report has only the keys the WolOS client sends (`kind`, `version`, `kernel`, `detail`,
//! cut to their lengths) and `at` (unix seconds, added here); any other key is dropped. Files older
//! than KEEP_DAYS go.
use axum::http::StatusCode;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest report body; a report is a summary, not a log dump.
pub const MAX_BODY: usize = 16 * 1024;
const KEEP_DAYS: u64 = 30;
/// Reports kept per day (at most 16 KiB each, so a day stays under ~320 MiB); past it, 429.
pub const DAY_CAP: u64 = 20_000;
/// Reports kept per kind and day: one kind (valid or invented) cannot use up the whole day.
pub const KIND_CAP: u64 = 5_000;
/// Posts one address may make per day, good or bad (a machine sends a few a day at most).
pub const ADDR_CAP: u32 = 100;
/// Distinct addresses counted per day; past it new addresses are refused until midnight.
const MAX_ADDRS: usize = 200_000;
/// The keys a stored report may have besides `kind`, and the longest value of each.
const KEYS: [(&str, usize); 3] = [("version", 32), ("kernel", 64), ("detail", 200)];

#[derive(Default)]
struct Day {
    day: u64,
    kinds: HashMap<String, u64>,
    /// Posts by address: keyed hash, never the address.
    addrs: HashMap<u64, u32>,
}

pub struct Reports {
    dir: PathBuf,
    today: Mutex<Day>,
    salt: RandomState,
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
            today: Mutex::new(Day {
                day,
                kinds: counts,
                addrs: HashMap::new(),
            }),
            salt: RandomState::new(),
        }
    }

    /// Keep one report from `from` (the client's address, only counted): 204; 400 when it is not an
    /// object with a valid `kind`; 429 past the address's, the kind's or the day's limit.
    pub fn post(&self, body: &[u8], from: &str) -> StatusCode {
        let at = now();
        let day = at / 86400;
        let mut t = self.today.lock().unwrap();
        if t.day != day {
            *t = Day {
                day,
                ..Day::default()
            };
            self.prune(day);
        }
        // Counted before the body is read: garbage costs the sender its share as well.
        let who = self.salt.hash_one(from);
        let known = t.addrs.contains_key(&who);
        if !known && t.addrs.len() >= MAX_ADDRS {
            return StatusCode::TOO_MANY_REQUESTS;
        }
        let n = t.addrs.entry(who).or_insert(0);
        if *n >= ADDR_CAP {
            return StatusCode::TOO_MANY_REQUESTS;
        }
        *n += 1;
        let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) else {
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
        // Only the client's own keys, as short strings: nothing else reaches the disk or the export.
        let mut kept = Map::new();
        kept.insert("kind".into(), kind.clone().into());
        for (key, max) in KEYS {
            if let Some(v) = m.get(key).and_then(Value::as_str) {
                kept.insert(key.into(), v.chars().take(max).collect::<String>().into());
            }
        }
        kept.insert("at".into(), at.into());
        let line = Value::Object(kept).to_string();
        if t.kinds.values().sum::<u64>() >= DAY_CAP
            || t.kinds.get(&kind).copied().unwrap_or(0) >= KIND_CAP
        {
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
        *t.kinds.entry(kind).or_insert(0) += 1;
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
        self.today.lock().unwrap().kinds.clone()
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
    format!("{}.jsonl", date(day))
}

/// `YYYY-MM-DD` for a day since the epoch (civil date, UTC).
pub fn date(day: u64) -> String {
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
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

    fn fresh(name: &str) -> Reports {
        let d =
            std::env::temp_dir().join(format!("wolos-cdn-report-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Reports::open(d)
    }

    #[test]
    fn unknown_keys_dropped() {
        let r = fresh("keys");
        let body = r#"{"kind":"crash","version":"0.12.1","kernel":"6.19","detail":"foot SIGSEGV","payload":"x","nested":{"a":1},"version2":"y","at":1}"#;
        assert_eq!(r.post(body.as_bytes(), "10.0.0.1"), StatusCode::NO_CONTENT);
        let long = format!(
            r#"{{"kind":"crash","detail":"{}","version":7}}"#,
            "d".repeat(500)
        );
        assert_eq!(r.post(long.as_bytes(), "10.0.0.1"), StatusCode::NO_CONTENT);
        let lines: Vec<Value> = r
            .read(1)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let mut keys: Vec<&String> = lines[0].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["at", "detail", "kernel", "kind", "version"]);
        assert_eq!(lines[0]["version"], "0.12.1");
        assert!(
            lines[0]["at"].as_u64().unwrap() > 1_700_000_000,
            "`at` is the server's, not the client's"
        );
        assert_eq!(
            lines[1]["detail"].as_str().unwrap().len(),
            200,
            "values are cut"
        );
        assert!(
            lines[1].get("version").is_none(),
            "a non-string value is dropped"
        );
    }

    #[test]
    fn the_101st_post_from_one_address_is_429() {
        let r = fresh("addr");
        let ok = br#"{"kind":"unit-failed","detail":"a.service"}"#;
        for i in 0..ADDR_CAP {
            assert_eq!(
                r.post(ok, "203.0.113.7"),
                StatusCode::NO_CONTENT,
                "post {i}"
            );
        }
        assert_eq!(r.post(ok, "203.0.113.7"), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            r.post(b"not json", "203.0.113.7"),
            StatusCode::TOO_MANY_REQUESTS,
            "garbage is counted too"
        );
        // Another address is not affected, and the flood did not fill the day.
        assert_eq!(r.post(ok, "203.0.113.8"), StatusCode::NO_CONTENT);
        assert_eq!(r.today()["unit-failed"], u64::from(ADDR_CAP) + 1);
    }

    #[test]
    fn one_kind_cannot_use_the_whole_day() {
        let r = fresh("kind");
        {
            let mut t = r.today.lock().unwrap();
            t.kinds.insert("flood".into(), KIND_CAP);
        }
        assert_eq!(
            r.post(br#"{"kind":"flood"}"#, "a"),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(r.post(br#"{"kind":"crash"}"#, "a"), StatusCode::NO_CONTENT);
    }

    #[test]
    fn the_address_is_never_written() {
        let r = fresh("noaddr");
        assert_eq!(
            r.post(br#"{"kind":"crash"}"#, "198.51.100.99"),
            StatusCode::NO_CONTENT
        );
        assert!(!r.read(1).contains("198.51.100.99"));
    }

    #[test]
    fn kinds() {
        assert!(valid_kind("unit-failed"));
        assert!(!valid_kind("Unit"));
        assert!(!valid_kind("a b"));
        assert!(!valid_kind(&"a".repeat(33)));
    }
}
