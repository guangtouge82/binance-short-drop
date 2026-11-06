//! CoinGecko market-cap membership filter. Mirrors mcap_rank.py: a symbol is
//! only kept on the short list when its base coin appears in the top ~2000 by
//! market cap. This drops stock / leveraged-ETF tokens (TZA, UVXY, SQQQ...)
//! that pump on the 24h ticker but are not crypto.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CG: &str = "https://api.coingecko.com/api/v3/coins/markets";
const PAGES: usize = 8;
const PER_PAGE: usize = 250;
const TTL: f64 = 6.0 * 3600.0;
const QUOTES: [&str; 4] = ["USDT", "USDC", "FDUSD", "BUSD"];

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

pub fn base_of(sym: &str) -> String {
    let mut s = sym.to_uppercase();
    s = s.split('_').next().unwrap_or("").to_string();
    for q in QUOTES {
        if s.ends_with(q) && s.len() > q.len() {
            s = s[..s.len() - q.len()].to_string();
            break;
        }
    }
    // strip multiplier prefixes like 1000PEPE / 1MBABYDOGE / 1000000...
    for pre in ["1000000", "100000", "10000", "1000", "100", "1M"] {
        if s.starts_with(pre) && s.len() > pre.len() && s.as_bytes()[pre.len()].is_ascii_uppercase() {
            s = s[pre.len()..].to_string();
            break;
        }
    }
    match s.as_str() {
        "BEAMX" => "BEAM".into(),
        "LUNA2" => "LUNA".into(),
        "RONIN" => "RON".into(),
        "MBABYDOGE" => "BABYDOGE".into(),
        _ => s,
    }
}

fn cache_path() -> PathBuf {
    let dir = std::env::var("SHORT_DROP_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(dir).join("mcap_cache.json")
}

fn fetch_page(page: usize) -> Result<Vec<Value>, String> {
    let url = format!(
        "{CG}?vs_currency=usd&order=market_cap_desc&per_page={PER_PAGE}&page={page}&sparkline=false"
    );
    // CoinGecko sits behind Cloudflare, which challenges the rustls TLS
    // fingerprint that ureq uses. curl on the VPS answers 200 reliably, so we
    // shell out to it for this one host.
    let mut last = String::from("err");
    for i in 0..4 {
        let out = std::process::Command::new("curl")
            .args([
                "-s", "--max-time", "25",
                "-A", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
                "-H", "Accept: application/json",
                &url,
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                match serde_json::from_slice::<Value>(&o.stdout) {
                    Ok(v) => {
                        if let Some(a) = v.as_array() {
                            return Ok(a.clone());
                        }
                        last = format!("non-array: {}", String::from_utf8_lossy(&o.stdout).chars().take(80).collect::<String>());
                    }
                    Err(e) => {
                        last = format!("parse: {e}; body={}", String::from_utf8_lossy(&o.stdout).chars().take(80).collect::<String>());
                    }
                }
            }
            Ok(o) => {
                last = format!("curl exit {:?}", o.status.code());
            }
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_secs(8 * (i as u64 + 1)));
    }
    Err(last)
}

/// Load (or refresh) the rank map: base coin symbol -> best (lowest) mcap rank.
pub fn load_ranks() -> HashMap<String, i64> {
    let path = cache_path();
    // try fresh cache first
    if let Ok(mut s) = fs::File::open(&path).map(|mut f| {
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        s
    }) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            let fetched = v.get("fetched_at").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if unix_now() - fetched < TTL {
                return extract_ranks(&v);
            }
        }
    }
    // rebuild
    let mut ranks: HashMap<String, i64> = HashMap::new();
    let mut got = 0usize;
    for p in 1..=PAGES {
        match fetch_page(p) {
            Ok(rows) => {
                if rows.is_empty() {
                    break;
                }
                for row in &rows {
                    let sym = row.get("symbol").and_then(|x| x.as_str()).unwrap_or("").to_uppercase();
                    let rk = row.get("market_cap_rank").and_then(|x| x.as_i64());
                    if sym.is_empty() || rk.is_none() {
                        continue;
                    }
                    let rk = rk.unwrap();
                    match ranks.get(&sym) {
                        Some(cur) if *cur <= rk => {}
                        _ => {
                            ranks.insert(sym, rk);
                        }
                    }
                }
                got += rows.len();
                thread::sleep(Duration::from_millis(6000));
            }
            Err(_) => {
                if p == 1 {
                    // total failure -> fall back to whatever stale cache we have
                    if let Ok(mut s) = fs::File::open(&path).map(|mut f| {
                        let mut s = String::new();
                        let _ = f.read_to_string(&mut s);
                        s
                    }) {
                        if let Ok(v) = serde_json::from_str::<Value>(&s) {
                            return extract_ranks(&v);
                        }
                    }
                    return HashMap::new();
                }
                break;
            }
        }
    }
    if got > 0 {
        let mut map = serde_json::Map::new();
        for (k, v) in &ranks {
            map.insert(k.clone(), serde_json::json!(v));
        }
        let out = serde_json::json!({"fetched_at": unix_now(), "coins": got, "ranks": map});
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, out.to_string());
    }
    ranks
}

fn extract_ranks(v: &Value) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    if let Some(m) = v.get("ranks").and_then(|x| x.as_object()) {
        for (k, val) in m {
            // cached form may be a bare number or {"rank": n}
            let rk = val
                .as_i64()
                .or_else(|| val.get("rank").and_then(|r| r.as_i64()));
            if let Some(r) = rk {
                out.insert(k.clone(), r);
            }
        }
    }
    out
}

/// True when the base coin of `symbol` is in the mcap top list.
pub fn is_listed(ranks: &HashMap<String, i64>, symbol: &str) -> bool {
    ranks.contains_key(&base_of(symbol))
}


/// Stock / leveraged-ETF tokens that trade as USDT perpetuals but are not
/// crypto. Used as a fallback filter when the CoinGecko list cannot be
/// fetched, so the monitor never collapses to an empty list just because the
/// market-cap source is unreachable.
pub const NON_CRYPTO_BASE: [&str; 24] = [
    "TZA", "UVXY", "SQQQ", "SOXS", "SOXL", "TQQQ", "SPXS", "SPXL", "APP", "NET",
    "SKDD", "SDS", "SH", "PSQ", "VXX", "SVXY", "JDST", "JNUG", "LABU", "LABD",
    "TECL", "TECS", "FNGU", "FNGD",
];

/// True when the symbol's base is a known stock/ETF token.
pub fn is_non_crypto_base(symbol: &str) -> bool {
    let b = base_of(symbol);
    NON_CRYPTO_BASE.iter().any(|x| *x == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_strips_quote_and_prefix() {
        assert_eq!(base_of("BTCUSDT"), "BTC");
        assert_eq!(base_of("1000PEPEUSDT"), "PEPE");
        assert_eq!(base_of("1MBABYDOGEUSDT"), "BABYDOGE");
        assert_eq!(base_of("BEAMXUSDT"), "BEAM");
        assert_eq!(base_of("BTCUSDT_261225"), "BTC");
        assert_eq!(base_of("ETHUSDC"), "ETH");
    }

    #[test]
    fn non_crypto_blacklist() {
        assert!(is_non_crypto_base("TZAUSDT"));
        assert!(is_non_crypto_base("UVXYUSDT"));
        assert!(is_non_crypto_base("SQQQUSDT"));
        assert!(is_non_crypto_base("APPUSDT"));
        assert!(!is_non_crypto_base("BTCUSDT"));
        assert!(!is_non_crypto_base("CYBERUSDT"));
    }

    #[test]
    fn listed_membership() {
        let mut r = HashMap::new();
        r.insert("BTC".to_string(), 1);
        r.insert("PEPE".to_string(), 30);
        assert!(is_listed(&r, "BTCUSDT"));
        assert!(is_listed(&r, "1000PEPEUSDT"));
        assert!(!is_listed(&r, "TZAUSDT"));
        assert!(!is_listed(&r, "UVXYUSDT"));
    }
}
