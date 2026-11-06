//! Short-rank dump monitor (做空榜暴跌10%监控) — VPS standalone service.
//!
//! Every minute, while a watch list is active, the service checks the current
//! Binance USD-M futures prices against the baselines captured when the list
//! was built. Any symbol that drops >= DUMP_PCT within the 1h window fires a
//! single Feishu card (once per symbol per list). The list itself is rebuilt
//! every REFRESH_SECS from a simplified self-contained short score.
//!
//! PUSH_FEISHU=1 enables dump-alert pushes; anything else is shadow (log only).
//! PUSH_RANK_FEISHU=1 enables the hourly short-rank Feishu card; default 0
//! keeps it in shadow (log the card body, do not POST).

use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod mcap;

use short_drop_monitor as core;
use core::{Candle, Ticker};

const HOSTS: [&str; 2] = ["https://fapi.binance.com", "https://www.binance.com"];
const LOG_MAX_BYTES: u64 = 20 * 1024 * 1024;
const LOG_KEEP_DAYS: i64 = 7;
const REFRESH_SECS: f64 = 3600.0;
const TICK_SECS: u64 = 60;

// ---- time helpers ----
fn tz() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap()
}
fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}
fn shanghai_now() -> String {
    Utc::now().with_timezone(&tz()).format("%Y-%m-%d %H:%M:%S").to_string()
}
fn shanghai_iso(t: f64) -> String {
    let dt = chrono::DateTime::<Utc>::from_timestamp(t as i64, 0).unwrap_or_else(Utc::now);
    dt.with_timezone(&tz()).format("%Y-%m-%d %H:%M:%S%z").to_string()
}
fn shanghai_date() -> String {
    Utc::now().with_timezone(&tz()).format("%Y-%m-%d").to_string()
}
fn shanghai_naive_today() -> NaiveDate {
    NaiveDate::parse_from_str(&shanghai_date(), "%Y-%m-%d").unwrap()
}

// ---- env / logging ----
fn push_feishu_enabled() -> bool {
    std::env::var("PUSH_FEISHU").unwrap_or_else(|_| "0".into()) == "1"
}

fn push_rank_feishu_enabled() -> bool {
    std::env::var("PUSH_RANK_FEISHU").unwrap_or_else(|_| "0".into()) == "1"
}

fn shanghai_hour_key() -> String {
    Utc::now().with_timezone(&tz()).format("%Y-%m-%d-%H").to_string()
}

fn shanghai_now_hm() -> String {
    Utc::now().with_timezone(&tz()).format("%Y-%m-%d %H:%M").to_string()
}

fn last_rank_hour_path() -> PathBuf {
    data_dir().join("last_rank_push_hour.txt")
}

fn load_last_rank_hour() -> Option<String> {
    fs::read_to_string(last_rank_hour_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_last_rank_hour(key: &str) {
    atomic_write(&last_rank_hour_path(), &format!("{key}\n"));
}

fn data_dir() -> PathBuf {
    std::env::var("SHORT_DROP_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

fn log_path() -> PathBuf {
    data_dir().join("monitor.log")
}

fn first_line_date(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let mut r = BufReader::new(f);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut r, &mut line).ok()?;
    // expect "YYYY-MM-DD ..." at start
    if line.len() >= 10 {
        let d = &line[..10];
        if NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok() {
            return Some(d.to_string());
        }
    }
    None
}

fn unique_rotated_path(path: &Path, date: &str) -> PathBuf {
    let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("log");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = parent.join(format!("{name}.{date}"));
    if !base.exists() {
        return base;
    }
    for n in 1..1000 {
        let cand = parent.join(format!("{name}.{date}.{n}"));
        if !cand.exists() {
            return cand;
        }
    }
    parent.join(format!("{name}.{date}.{}", unix_now() as u64))
}

fn prune_old_rotated_logs(path: &Path) {
    let Some(name) = path.file_name().and_then(|x| x.to_str()) else { return };
    let Some(parent) = path.parent() else { return };
    let Ok(rd) = fs::read_dir(parent) else { return };
    let cutoff = shanghai_naive_today() - ChronoDuration::days(LOG_KEEP_DAYS);
    let prefix = format!("{name}.");
    for ent in rd.flatten() {
        let fname = ent.file_name();
        let Some(fname) = fname.to_str() else { continue };
        if !fname.starts_with(&prefix) {
            continue;
        }
        let rest = &fname[prefix.len()..];
        let date_part = rest.split('.').next().unwrap_or("");
        let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") else { continue };
        if d < cutoff {
            let _ = fs::remove_file(ent.path());
        }
    }
}

fn rotate_log_file(path: &Path, date: &str) {
    if !path.exists() {
        return;
    }
    let dest = unique_rotated_path(path, date);
    if fs::rename(path, &dest).is_err() {
        let _ = fs::File::create(path);
    }
    prune_old_rotated_logs(path);
}

fn maybe_rotate_log(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        prune_old_rotated_logs(path);
        return;
    };
    if !meta.is_file() {
        return;
    }
    let today = shanghai_date();
    let size = meta.len();
    let first_date = first_line_date(path);
    let need_daily = first_date.as_ref().map(|d| d.as_str() < today.as_str()).unwrap_or(false);
    let need_size = size >= LOG_MAX_BYTES;
    if need_daily || need_size {
        let rot = first_date.unwrap_or(today);
        rotate_log_file(path, &rot);
    } else {
        prune_old_rotated_logs(path);
    }
}

fn log_line(msg: &str) {
    let path = log_path();
    maybe_rotate_log(&path);
    let line = format!("{} {}\n", shanghai_now(), msg);
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
    print!("{line}");
}

// ---- http ----
fn http_get(path: &str) -> Result<Value, String> {
    let mut last = String::from("no hosts");
    for attempt in 0..3 {
        for host in HOSTS {
            let url = format!("{host}{path}");
            let resp = ureq::get(&url)
                .set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
                .set("Accept", "application/json")
                .timeout(Duration::from_secs(20))
                .call();
            match resp {
                Ok(r) => {
                    return r.into_json::<Value>().map_err(|e| e.to_string());
                }
                Err(ureq::Error::Status(418, _)) => return Err("HTTP 418 ip-ban".into()),
                Err(e) => last = e.to_string(),
            }
        }
        thread::sleep(Duration::from_secs(1 + attempt));
    }
    Err(last)
}

fn parse_candles(v: &Value) -> Vec<Candle> {
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for k in arr {
            let g = |i: usize| k.get(i).and_then(|x| {
                x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            });
            let (Some(ts), Some(o), Some(h), Some(l), Some(c)) =
                (k.get(0).and_then(|x| x.as_i64()), g(1), g(2), g(3), g(4))
            else {
                continue;
            };
            out.push(Candle { ts: ts / 1000, open: o, high: h, low: l, close: c });
        }
    }
    out
}

fn fetch_klines(sym: &str, interval: &str, limit: usize) -> Vec<Candle> {
    match http_get(&format!("/fapi/v1/klines?symbol={sym}&interval={interval}&limit={limit}")) {
        Ok(v) => parse_candles(&v),
        Err(_) => Vec::new(),
    }
}

fn fetch_tickers() -> Vec<Ticker> {
    let Ok(v) = http_get("/fapi/v1/ticker/24hr") else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for t in arr {
            let Some(symbol) = t.get("symbol").and_then(|x| x.as_str()) else { continue };
            let pct = t.get("priceChangePercent").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let qv = t.get("quoteVolume").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            out.push(Ticker::new(symbol, pct, qv));
        }
    }
    out
}

fn fetch_price_map(symbols: &[String]) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    let wanted: std::collections::HashSet<&str> = symbols.iter().map(|s| s.as_str()).collect();
    // bulk endpoints in order
    for (path, field) in [
        ("/fapi/v1/ticker/price".to_string(), "price"),
        ("/fapi/v1/ticker/24hr".to_string(), "lastPrice"),
        ("/fapi/v1/premiumIndex".to_string(), "markPrice"),
    ] {
        if wanted.iter().all(|s| out.contains_key(*s)) {
            break;
        }
        if let Ok(v) = http_get(&path) {
            absorb(&v, field, &wanted, &mut out);
        }
    }
    for sym in symbols {
        if out.contains_key(sym) {
            continue;
        }
        for (path, field) in [
            ("/fapi/v1/ticker/price", "price"),
            ("/fapi/v1/ticker/24hr", "lastPrice"),
            ("/fapi/v1/premiumIndex", "markPrice"),
        ] {
            if let Ok(v) = http_get(&format!("{path}?symbol={sym}")) {
                absorb(&v, field, &wanted, &mut out);
            }
            if out.contains_key(sym) {
                break;
            }
        }
    }
    out
}

fn absorb(v: &Value, field: &str, wanted: &std::collections::HashSet<&str>, out: &mut HashMap<String, f64>) {
    let rows: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        _ => return,
    };
    for row in rows {
        let Some(sym) = row.get("symbol").and_then(|x| x.as_str()) else { continue };
        if !wanted.contains(sym) || out.contains_key(sym) {
            continue;
        }
        let val = row.get(field).and_then(|x| {
            x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        });
        if let Some(p) = val {
            if p > 0.0 {
                out.insert(sym.to_string(), p);
            }
        }
    }
}

// ---- watch list state ----
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SymRow {
    symbol: String,
    baseline: f64,
    short_score: Option<i32>,
    #[serde(default)]
    alerted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    alerted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alert_drop_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alert_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WatchList {
    saved_at: String,
    saved_at_epoch: f64,
    expires_at: String,
    expires_at_epoch: f64,
    dump_pct: f64,
    symbols: Vec<SymRow>,
    #[serde(default)]
    alerts: Vec<Value>,
    #[serde(default)]
    skipped_no_price: Vec<String>,
}

fn watchlist_path() -> PathBuf {
    data_dir().join("watchlist.json")
}

fn atomic_write(path: &Path, contents: &str) {
    if let Some(p) = path.parent() {
        let _ = fs::create_dir_all(p);
    }
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, contents).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn save_watchlist(wl: &WatchList) {
    if let Ok(s) = serde_json::to_string_pretty(wl) {
        atomic_write(&watchlist_path(), &s);
    }
}

fn load_watchlist() -> Option<WatchList> {
    let mut s = String::new();
    fs::File::open(watchlist_path()).ok()?.read_to_string(&mut s).ok()?;
    serde_json::from_str(&s).ok()
}

// ---- scoring: build the short list from public data ----
#[derive(Debug, Clone)]
struct ScoreDetail {
    symbol: String,
    short_score: i32,
    pct: f64,
    premium: f64,
    peak_gap_pct: f64,
    dist_high_pct: f64,
    atr_expansion: f64,
    atr_pct: f64,
    bear_5m: bool,
}

fn score_symbol(t: &Ticker) -> Option<ScoreDetail> {
    let bars4h = fetch_klines(&t.symbol, "4h", 100);
    let bars5m = fetch_klines(&t.symbol, "5m", 150);
    let bars1h = fetch_klines(&t.symbol, "1h", 60);
    if bars4h.is_empty() || bars5m.is_empty() {
        return None;
    }
    let premium = core::premium_pct(&bars4h, 42);
    let gap = core::double_top_gap_pct(&bars4h, 3);
    let hi = bars4h.iter().map(|b| b.high).fold(f64::NEG_INFINITY, f64::max);
    let last = bars4h.last().unwrap().close;
    let dist_high = if hi > 0.0 { (hi - last) / hi * 100.0 } else { 0.0 };
    // ATR expansion: recent ATR vs earlier baseline (percentage change).
    let atr_recent = core::atr_pct(&bars1h, 14);
    let atr_base = if bars1h.len() > 34 {
        core::atr_pct(&bars1h[..bars1h.len() - 20], 14)
    } else {
        atr_recent
    };
    let atr_expansion = if atr_base > 0.0 {
        (atr_recent - atr_base) / atr_base * 100.0
    } else {
        0.0
    };
    let bear = core::bearish_5m(&bars5m, 3);
    let score = core::short_score(t.price_change_percent, premium, gap, dist_high, atr_expansion, bear);
    Some(ScoreDetail {
        symbol: t.symbol.clone(),
        short_score: score,
        pct: t.price_change_percent,
        premium,
        peak_gap_pct: gap,
        dist_high_pct: dist_high,
        atr_expansion,
        atr_pct: atr_recent,
        bear_5m: bear,
    })
}

fn refresh_watchlist(prices_now: &HashMap<String, f64>) -> Option<WatchList> {
    let tickers = fetch_tickers();
    if tickers.is_empty() {
        log_line("REFRESH failed: no tickers");
        return None;
    }
    let cands = core::pick_candidates(&tickers, core::CANDIDATES);
    log_line(&format!("REFRESH candidates={}", cands.len()));
    let mut scored: Vec<ScoreDetail> = Vec::new();
    for t in &cands {
        if let Some(s) = score_symbol(t) {
            scored.push(s);
        }
    }
    scored.sort_by(|a, b| b.short_score.cmp(&a.short_score));
    // Drop non-crypto tokens (stock / leveraged ETF) using the CoinGecko
    // market-cap list, exactly like the original short_rank does.
    let ranks = mcap::load_ranks();
    let before = scored.len();
    let scored: Vec<ScoreDetail> = if ranks.is_empty() {
        // CoinGecko unreachable: fall back to the static stock/ETF blacklist so
        // the monitor keeps working instead of collapsing to an empty list.
        log_line("REFRESH coingecko empty, using non-crypto blacklist fallback");
        scored
            .into_iter()
            .filter(|s| !mcap::is_non_crypto_base(&s.symbol))
            .collect()
    } else {
        scored
            .into_iter()
            .filter(|s| mcap::is_listed(&ranks, &s.symbol))
            .collect()
    };
    log_line(&format!("REFRESH mcap filter {} -> {} (ranks={})", before, scored.len(), ranks.len()));
    if scored.is_empty() {
        log_line("REFRESH produced empty list, keeping previous watchlist");
        return None;
    }
    let top: Vec<ScoreDetail> = scored.into_iter().take(core::TOP_N).collect();
    let now = unix_now();
    let expires = now + core::WATCH_SECS;
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    let mut rank_rows: Vec<core::RankRow> = Vec::new();
    for s in &top {
        let px = prices_now.get(&s.symbol).copied().or_else(|| {
            fetch_price_map(std::slice::from_ref(&s.symbol)).get(&s.symbol).copied()
        });
        match px {
            Some(p) if p > 0.0 => {
                rows.push(SymRow {
                    symbol: s.symbol.clone(),
                    baseline: p,
                    short_score: Some(s.short_score),
                    alerted: false,
                    alerted_at: None,
                    alert_drop_pct: None,
                    alert_price: None,
                });
                rank_rows.push(core::RankRow {
                    symbol: s.symbol.clone(),
                    short_score: s.short_score,
                    pct: s.pct,
                    mcap_rank: mcap::rank_of(&ranks, &s.symbol),
                    premium: s.premium,
                    peak_gap_pct: s.peak_gap_pct,
                    dist_high_pct: s.dist_high_pct,
                    atr_expansion: s.atr_expansion,
                    atr_pct: s.atr_pct,
                    bear_5m: s.bear_5m,
                });
            }
            _ => skipped.push(s.symbol.clone()),
        }
    }
    let wl = WatchList {
        saved_at: shanghai_iso(now),
        saved_at_epoch: now,
        expires_at: shanghai_iso(expires),
        expires_at_epoch: expires,
        dump_pct: core::DUMP_PCT,
        symbols: rows,
        alerts: Vec::new(),
        skipped_no_price: skipped,
    };
    save_watchlist(&wl);
    log_line(&format!(
        "REFRESH saved symbols={} skipped={} top={:?}",
        wl.symbols.len(),
        wl.skipped_no_price.len(),
        wl.symbols.iter().take(3).map(|s| s.symbol.clone()).collect::<Vec<_>>()
    ));
    maybe_push_rank_card(&rank_rows);
    Some(wl)
}

// ---- feishu ----
fn feishu_webhook() -> Option<String> {
    if let Ok(w) = std::env::var("FEISHU_WEBHOOK_URL") {
        if !w.trim().is_empty() {
            return Some(w.trim().to_string());
        }
    }
    let path = std::env::var("FEISHU_WEBHOOK_FILE").unwrap_or_else(|_| "feishu_webhook_url.txt".into());
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn post_feishu(payload: &Value) -> (i32, String) {
    let Some(wh) = feishu_webhook() else {
        return (-1, "no webhook configured".into());
    };
    match ureq::post(&wh)
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(20))
        .send_json(payload)
    {
        Ok(r) => {
            let code = r.status() as i32;
            let body = r.into_string().unwrap_or_default();
            (code, body)
        }
        Err(ureq::Error::Status(code, r)) => (code as i32, r.into_string().unwrap_or_default()),
        Err(e) => (-1, e.to_string()),
    }
}

fn maybe_push_rank_card(rows: &[core::RankRow]) {
    if rows.is_empty() {
        return;
    }
    let hour = shanghai_hour_key();
    let last = load_last_rank_hour();
    if !core::should_send_rank(last.as_deref(), &hour) {
        log_line(&format!("RANK skip already pushed hour={hour}"));
        return;
    }
    let now_hm = shanghai_now_hm();
    let body = core::format_rank_md(rows, &now_hm);
    let title = core::rank_feishu_card_title(&now_hm);
    // Record the hour even in shadow mode so a restart does not re-emit.
    save_last_rank_hour(&hour);
    if !push_rank_feishu_enabled() {
        // Keep the shadow log readable: one line with title, then the body.
        log_line(&format!("SHADOW rank card title={title} chars={}", body.chars().count()));
        for line in body.lines() {
            log_line(&format!("SHADOW rank | {line}"));
        }
        return;
    }
    let payload = json!({
        "msg_type": "interactive",
        "card": {
            "config": {"wide_screen_mode": true},
            "header": {"title": {"tag": "plain_text", "content": title}, "template": "red"},
            "elements": [{"tag": "div", "text": {"tag": "lark_md", "content": body}}],
        }
    });
    let (code, resp) = post_feishu(&payload);
    log_line(&format!(
        "RANK pushed hour={hour} feishu_http={} resp={}",
        code,
        &resp.chars().take(120).collect::<String>()
    ));
}

fn send_dump_alert(row: &SymRow, current: f64, drop_pct: f64, threshold: f64) {
    let now_str = shanghai_now();
    let body = core::feishu_card_body(
        &row.symbol,
        row.short_score,
        row.baseline,
        current,
        drop_pct,
        &now_str,
        threshold,
    );
    let title = core::feishu_card_title(&now_str);
    if !push_feishu_enabled() {
        log_line(&format!(
            "SHADOW alert {} drop={:.2}% baseline={} current={}",
            row.symbol, drop_pct, row.baseline, current
        ));
        return;
    }
    let payload = json!({
        "msg_type": "interactive",
        "card": {
            "config": {"wide_screen_mode": true},
            "header": {"title": {"tag": "plain_text", "content": title}, "template": "red"},
            "elements": [{"tag": "div", "text": {"tag": "lark_md", "content": body}}],
        }
    });
    let (code, resp) = post_feishu(&payload);
    log_line(&format!("ALERT {} drop={:.2}% feishu_http={} resp={}", row.symbol, drop_pct, code, &resp.chars().take(120).collect::<String>()));
}

// ---- one check pass ----
fn check_once(wl: &mut WatchList) {
    let now = unix_now();
    if core::is_expired(now, wl.expires_at_epoch) {
        log_line("CHECK watchlist expired");
        return;
    }
    let symbols: Vec<String> = wl.symbols.iter().map(|s| s.symbol.clone()).collect();
    let prices = fetch_price_map(&symbols);
    let mut fired = 0;
    let threshold = wl.dump_pct;
    for row in wl.symbols.iter_mut() {
        let cur = prices.get(&row.symbol).copied().unwrap_or(0.0);
        if core::should_alert(row.alerted, row.baseline, cur, threshold) {
            let dp = core::drop_pct(row.baseline, cur);
            send_dump_alert(row, cur, dp, threshold);
            row.alerted = true;
            row.alerted_at = Some(shanghai_iso(now));
            row.alert_drop_pct = Some((dp * 10000.0).round() / 10000.0);
            row.alert_price = Some(cur);
            wl.alerts.push(json!({
                "symbol": row.symbol,
                "baseline": row.baseline,
                "current": cur,
                "drop_pct": (dp * 10000.0).round() / 10000.0,
                "short_score": row.short_score,
                "at": shanghai_iso(now),
            }));
            fired += 1;
        }
    }
    if fired > 0 {
        save_watchlist(wl);
    }
    log_line(&format!(
        "CHECK symbols={} priced={} fired={} alerted_total={}",
        wl.symbols.len(),
        prices.len(),
        fired,
        wl.symbols.iter().filter(|s| s.alerted).count()
    ));
}

fn main() {
    log_line(&format!(
        "START short_drop_monitor push_feishu={} push_rank_feishu={} dir={:?}",
        push_feishu_enabled(),
        push_rank_feishu_enabled(),
        data_dir()
    ));
    let mut last_refresh = 0.0f64;
    loop {
        let now = unix_now();
        let need_refresh = match load_watchlist() {
            None => true,
            Some(wl) => {
                core::is_expired(now, wl.expires_at_epoch) || (now - last_refresh >= REFRESH_SECS)
            }
        };
        if need_refresh {
            if refresh_watchlist(&HashMap::new()).is_some() {
                last_refresh = unix_now();
            } else {
                thread::sleep(Duration::from_secs(30));
                continue;
            }
        }
        if let Some(mut wl) = load_watchlist() {
            check_once(&mut wl);
        }
        thread::sleep(Duration::from_secs(TICK_SECS));
    }
}
