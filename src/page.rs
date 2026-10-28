//! The status page at `/`: storage against the budget, hit rate, mirrors, recently used files.
use crate::cache::Cache;
use std::sync::atomic::Ordering;

fn human(b: u64) -> String {
    let mut v = b as f64;
    for u in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return format!("{v:.1} {u}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn stats(c: &Cache, mirrors: &[(String, String)], origin: &str) -> serde_json::Value {
    serde_json::json!({
        "used": c.used(), "max": c.max, "files": c.count(),
        "hits": c.hits.load(Ordering::Relaxed), "misses": c.misses.load(Ordering::Relaxed),
        "sent": c.sent.load(Ordering::Relaxed),
        "origin": origin,
        "mirrors": mirrors.iter().map(|(p, u)| serde_json::json!({"prefix": p, "url": u})).collect::<Vec<_>>(),
    })
}

pub fn html(c: &Cache, mirrors: &[(String, String)], origin: &str) -> String {
    let (used, max) = (c.used(), c.max.max(1));
    let (hits, misses) = (
        c.hits.load(Ordering::Relaxed),
        c.misses.load(Ordering::Relaxed),
    );
    let rate = (hits * 100).checked_div(hits + misses).unwrap_or(0);
    let pct = (used * 100 / max).min(100);
    let mut rows = String::new();
    for (p, u) in mirrors {
        rows += &format!(
            "<tr><td><code>/{}/</code></td><td>{}</td></tr>",
            esc(p),
            esc(u)
        );
    }
    rows += &format!(
        "<tr><td><code>/</code> (rest)</td><td>{}</td></tr>",
        esc(origin)
    );
    let mut recent = String::new();
    for (f, size, _) in c.recent(15) {
        recent += &format!(
            "<tr><td><code>{}</code></td><td class=n>{}</td></tr>",
            esc(&f),
            human(size)
        );
    }
    format!(
        r#"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>WolOS CDN</title><meta http-equiv=refresh content=15>
<style>
:root{{--bg:#f6f6f8;--card:#fff;--fg:#1d1d1f;--mute:#6e6e73;--line:#e5e5ea;--acc:#0a84ff}}
@media (prefers-color-scheme:dark){{:root{{--bg:#111113;--card:#1c1c1e;--fg:#f5f5f7;--mute:#98989d;--line:#2c2c2e}}}}
body{{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 -apple-system,system-ui,sans-serif}}
main{{max-width:880px;margin:0 auto;padding:32px 16px}}h1{{font-size:28px;margin:0 0 4px}}p{{color:var(--mute);margin:0 0 24px}}
.g{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px;margin-bottom:16px}}
.c{{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:16px}}
.k{{color:var(--mute);font-size:13px}}.v{{font-size:24px;font-weight:600;margin-top:4px}}
.bar{{height:10px;background:var(--line);border-radius:5px;overflow:hidden;margin-top:10px}}.bar i{{display:block;height:100%;background:var(--acc);width:{pct}%}}
table{{width:100%;border-collapse:collapse}}td{{padding:7px 4px;border-top:1px solid var(--line);word-break:break-all}}td.n{{text-align:right;white-space:nowrap;color:var(--mute)}}
h2{{font-size:17px;margin:0 0 8px}}code{{font:13px ui-monospace,monospace}}
</style><main>
<h1>WolOS CDN</h1><p>Packages, ISOs and mirrors for WolOS. pacman: <code>Server = https://&lt;this host&gt;/$repo-&lt;release&gt;</code></p>
<div class=g>
<div class=c><div class=k>Storage</div><div class=v>{used_h}</div><div class=k>of {max_h} ({pct}%)</div><div class=bar><i></i></div></div>
<div class=c><div class=k>Cache hit rate</div><div class=v>{rate}%</div><div class=k>{hits} hits, {misses} fetched</div></div>
<div class=c><div class=k>Files</div><div class=v>{files}</div><div class=k>{sent} served</div></div>
</div>
<div class=c style="margin-bottom:16px"><h2>Sources</h2><table>{rows}</table></div>
<div class=c><h2>Recently used</h2><table>{recent}</table></div>
</main></html>"#,
        used_h = human(used),
        max_h = human(c.max),
        files = c.count(),
        sent = human(c.sent.load(Ordering::Relaxed)),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_read_like_people_write_them() {
        assert_eq!(super::human(512), "512.0 B");
        assert_eq!(super::human(7 << 30), "7.0 GB");
        assert_eq!(super::esc("<a&b>"), "&lt;a&amp;b&gt;");
    }
}
