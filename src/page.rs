//! The status page at `/` (storage against the budget, hit rate, mirrors, recently used files)
//! and the releases page at `/releases`. Plain HTML, no scripts, the same inline tokens.
use crate::cache::Cache;
use crate::releases::Release;
use std::sync::atomic::Ordering;

pub fn human(b: u64) -> String {
    let mut v = b as f64;
    for u in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return format!("{v:.1} {u}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

/// Text and attribute values (quoted with `"`).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The WolOS tokens and the shared layout (cards, tables, code), light and dark.
const BASE_CSS: &str = r#":root{--bg:#f6f6f8;--card:#fff;--fg:#1d1d1f;--mute:#6e6e73;--line:#e5e5ea;--acc:#0a84ff}
@media (prefers-color-scheme:dark){:root{--bg:#111113;--card:#1c1c1e;--fg:#f5f5f7;--mute:#98989d;--line:#2c2c2e}}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 -apple-system,system-ui,sans-serif}
main{max-width:880px;margin:0 auto;padding:32px 16px}h1{font-size:28px;margin:0 0 4px}p{color:var(--mute);margin:0 0 24px}
.c{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:16px}
.k{color:var(--mute);font-size:13px}a{color:var(--acc)}
table{width:100%;border-collapse:collapse}td{padding:7px 4px;border-top:1px solid var(--line);word-break:break-all}td.n{text-align:right;white-space:nowrap;color:var(--mute)}
h2{font-size:17px;margin:0 0 8px}code{font:13px ui-monospace,monospace}"#;

pub fn stats(c: &Cache, mirrors: &[(String, String)], origin: &str) -> serde_json::Value {
    serde_json::json!({
        "used": c.used(), "max": c.max, "files": c.count(),
        "hits": c.hits.load(Ordering::Relaxed), "misses": c.misses.load(Ordering::Relaxed),
        "sent": c.sent.load(Ordering::Relaxed),
        "origin": origin,
        "mirrors": mirrors.iter().map(|(p, u)| serde_json::json!({"prefix": p, "url": u})).collect::<Vec<_>>(),
    })
}

/// `reports`: today's error reports by kind.
pub fn html(
    c: &Cache,
    mirrors: &[(String, String)],
    origin: &str,
    reports: &std::collections::HashMap<String, u64>,
) -> String {
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
    let mut kinds: Vec<_> = reports.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let mut errs = String::new();
    for (k, n) in &kinds {
        errs += &format!(
            "<tr><td><code>{}</code></td><td class=n>{n}</td></tr>",
            esc(k)
        );
    }
    if errs.is_empty() {
        errs = "<tr><td>None today</td></tr>".into();
    }
    let total: u64 = reports.values().sum();
    format!(
        r#"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>WolOS CDN</title><meta http-equiv=refresh content=15>
<style>
{BASE_CSS}
.g{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px;margin-bottom:16px}}
.v{{font-size:24px;font-weight:600;margin-top:4px}}
.bar{{height:10px;background:var(--line);border-radius:5px;overflow:hidden;margin-top:10px}}.bar i{{display:block;height:100%;background:var(--acc);width:{pct}%}}
</style><main>
<h1>WolOS CDN</h1><p>Packages, ISOs and mirrors for WolOS. pacman: <code>Server = https://&lt;this host&gt;/$repo-&lt;release&gt;</code><br>
<a href=/releases>Download WolOS</a>: every release with its checksum (<a href=/releases.json>releases.json</a>)</p>
<div class=g>
<div class=c><div class=k>Storage</div><div class=v>{used_h}</div><div class=k>of {max_h} ({pct}%)</div><div class=bar><i></i></div></div>
<div class=c><div class=k>Cache hit rate</div><div class=v>{rate}%</div><div class=k>{hits} hits, {misses} fetched</div></div>
<div class=c><div class=k>Files</div><div class=v>{files}</div><div class=k>{sent} served</div></div>
<div class=c><div class=k>Error reports today</div><div class=v>{total}</div><div class=k>from machines that send them</div></div>
</div>
<div class=c style="margin-bottom:16px"><h2>Errors today</h2><table>{errs}</table></div>
<div class=c style="margin-bottom:16px"><h2>Sources</h2><table>{rows}</table></div>
<div class=c><h2>Recently used</h2><table>{recent}</table></div>
</main></html>"#,
        used_h = human(used),
        max_h = human(c.max),
        files = c.count(),
        sent = human(c.sent.load(Ordering::Relaxed)),
    )
}

/// The releases page: newest first, ISO links (or the GitHub mirror once evicted), size,
/// sha256 with a verify line, and how to write the USB stick.
pub fn releases(list: &[Release]) -> String {
    let mut cards = String::new();
    for (i, r) in list.iter().enumerate() {
        let dir = format!("/{}/", r.folder);
        let date = r.date.as_deref().map(esc).unwrap_or_default();
        let latest = if i == 0 {
            " <span class=t>Latest</span>"
        } else {
            ""
        };
        let size = r
            .size
            .map(|n| format!(" <span class=k>{}</span>", human(n)));
        let size = size.unwrap_or_default();
        // The file name the verify line checks: the ISO here, else the mirror's last segment.
        let file = r
            .iso
            .clone()
            .or_else(|| r.mirror.as_deref()?.rsplit('/').next().map(String::from));
        let get = match (&r.iso, &r.mirror) {
            (Some(iso), _) => format!(
                "<a class=dl href=\"{d}{i}\">{i}</a>{size}",
                d = esc(&dir),
                i = esc(iso)
            ),
            (None, Some(m)) => format!(
                "No longer kept on this host: <a class=dl href=\"{m}\">{f}</a>{size} from the GitHub release",
                m = esc(m),
                f = esc(file.as_deref().unwrap_or("the ISO"))
            ),
            (None, None) => "No longer kept on this host, and release.json names no mirror".into(),
        };
        let mut files = Vec::new();
        if let Some(f) = &r.sha_file {
            files.push(format!(
                "<a href=\"{}{}\">{}</a>",
                esc(&dir),
                esc(f),
                esc(f)
            ));
        }
        files.push(format!(
            "<a href=\"{}release.json\">release.json</a>",
            esc(&dir)
        ));
        if let Some(n) = &r.notes_url {
            files.push(format!("<a href=\"{}\">Release notes</a>", esc(n)));
        }
        let mut rows = String::new();
        if let Some(h) = &r.sha256 {
            rows += &format!(
                "<tr><td class=h>SHA-256</td><td><code>{}</code></td></tr>",
                esc(h)
            );
        }
        rows += &format!(
            "<tr><td class=h>Files</td><td>{}</td></tr>",
            files.join(" · ")
        );
        let verify = match (&r.sha256, &file) {
            (Some(h), Some(f)) => format!(
                "<pre><code>echo \"{}  {}\" | sha256sum -c</code></pre>",
                esc(h),
                esc(f)
            ),
            _ => String::new(),
        };
        cards += &format!(
            "<div class=c><h2>WolOS {v}{latest} <span class=k>{date}</span></h2><p class=l>{get}</p><table>{rows}</table>{verify}</div>\n",
            v = esc(&r.version_s),
        );
    }
    if cards.is_empty() {
        cards = "<div class=c>No releases on this host yet.</div>".into();
    }
    format!(
        r#"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>WolOS releases</title>
<style>
{BASE_CSS}
.c{{margin-bottom:16px}}p.l{{color:var(--fg);margin:0 0 8px}}a.dl{{font-weight:600}}td.h{{color:var(--mute);width:84px;white-space:nowrap;vertical-align:top}}
.t{{font-size:12px;font-weight:600;color:#fff;background:var(--acc);border-radius:6px;padding:1px 6px;vertical-align:middle}}
pre{{margin:10px 0 0;padding:10px 12px;background:var(--bg);border:1px solid var(--line);border-radius:8px;overflow-x:auto}}
</style><main>
<h1>WolOS releases</h1><p>One ISO for every 64-bit PC with UEFI. Newest first; also as <a href=/releases.json>releases.json</a>. <a href=/>CDN status</a></p>
{cards}<div class=c><h2>Check the download</h2><p class=l>Linux: the line under each release prints <code>OK</code>. macOS: <code>shasum -a 256 wolos-*.iso</code>. Windows (PowerShell): <code>Get-FileHash .\wolos-*.iso -Algorithm SHA256</code>. The digest must match the SHA-256 above.</p></div>
<div class=c><h2>Write the USB stick</h2><p class=l>Use a stick of 4 GB or more; its contents are replaced. balenaEtcher, Fedora Media Writer or Rufus (DD mode) all work. On Linux: <code>sudo dd if=wolos-*.iso of=/dev/sdX bs=4M status=progress oflag=sync</code>, with your stick as <code>sdX</code> (see <code>lsblk</code>). Then start the PC from the stick in UEFI mode.</p></div>
</main></html>"#
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_read_like_people_write_them() {
        assert_eq!(super::human(512), "512.0 B");
        assert_eq!(super::human(7 << 30), "7.0 GB");
        assert_eq!(super::esc("<a&b>"), "&lt;a&amp;b&gt;");
        assert_eq!(super::esc("a\"b"), "a&quot;b");
    }
}
