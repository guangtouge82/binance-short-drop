//! Pure logic for the short-rank dump (暴跌) monitor.
//!
//! Everything in this module is free of IO so it can be unit-tested:
//! candidate selection, a simplified short score, drop-percentage math,
//! trigger / dedupe decisions, watchlist expiry and the Feishu card body.

use serde::{Deserialize, Serialize};

pub const DUMP_PCT: f64 = 10.0;
pub const WATCH_SECS: f64 = 3600.0;
pub const CANDIDATES: usize = 30;
pub const TOP_N: usize = 15;

/// Symbols we never rank: leveraged tokens, quarterly deliveries, fiat-quoted.
pub const SKIP_SUFFIX: [&str; 4] = ["UPUSDT", "DOWNUSDT", "BULLUSDT", "BEARUSDT"];
pub const FIAT_QUOTES: [&str; 20] = [
    "TRY", "BRL", "EUR", "GBP", "BIDR", "IDRT", "RUB", "UAH", "ZAR", "NGN", "ARS", "PLN", "RON",
    "JPY", "AUD", "CAD", "CHF", "MXN", "COP", "CZK",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ticker {
    pub symbol: String,
    pub price_change_percent: f64,
    pub quote_volume: f64,
}

impl Ticker {
    pub fn new(symbol: &str, pct: f64, qv: f64) -> Self {
        Self {
            symbol: symbol.to_string(),
            price_change_percent: pct,
            quote_volume: qv,
        }
    }
}

/// Keep only plain spot-margined USDT perpetuals that went UP on the 24h ticker,
/// ranked by 24h percent change, descending. Mirrors short_rank.pick_candidates.
pub fn pick_candidates(tickers: &[Ticker], n: usize) -> Vec<Ticker> {
    let mut usdt: Vec<Ticker> = tickers
        .iter()
        .filter(|t| {
            let s = t.symbol.as_str();
            if !s.ends_with("USDT") {
                return false;
            }
            if SKIP_SUFFIX.iter().any(|suf| s.ends_with(suf)) {
                return false;
            }
            if s.contains('_') {
                return false;
            }
            // Fiat composite like BTCEUR etc: reject when a fiat code appears
            // right before the USDT quote, plus any USD1 stable-quoted symbol.
            if s.contains("USD1") {
                return false;
            }
            let base = &s[..s.len() - 4];
            if FIAT_QUOTES.iter().any(|q| base.ends_with(q)) {
                return false;
            }
            t.price_change_percent > 0.0
        })
        .cloned()
        .collect();
    usdt.sort_by(|a, b| {
        b.price_change_percent
            .partial_cmp(&a.price_change_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // Prefer the hot list (>=5%); if there are not enough, top it up with the
    // rest of the positive gainers (still never the losers).
    let hot: Vec<Ticker> = usdt
        .iter()
        .filter(|t| t.price_change_percent >= 5.0)
        .cloned()
        .collect();
    if hot.len() >= n {
        hot.into_iter().take(n).collect()
    } else {
        usdt.into_iter().take(n).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// ATR(14) as a percentage of the last close; 0.0 when data is unusable.
pub fn atr_pct(bars: &[Candle], period: usize) -> f64 {
    if bars.len() < period + 1 {
        return 0.0;
    }
    let mut sum = 0.0;
    let mut count = 0;
    for i in (bars.len() - period)..bars.len() {
        let c = bars[i];
        let prev_close = bars[i - 1].close;
        let tr = (c.high - c.low)
            .max((c.high - prev_close).abs())
            .max((c.low - prev_close).abs());
        sum += tr;
        count += 1;
    }
    let last = bars.last().unwrap().close;
    if count == 0 || last <= 0.0 {
        return 0.0;
    }
    (sum / count as f64) / last * 100.0
}

/// Percentage of the recent lookback range that the last close sits at
/// (0 = at the low, 100 = at the high). This is the "Premium / discount" proxy.
pub fn premium_pct(bars: &[Candle], lookback: usize) -> f64 {
    if bars.is_empty() {
        return 50.0;
    }
    let start = bars.len().saturating_sub(lookback);
    let w = &bars[start..];
    let hi = w.iter().map(|b| b.high).fold(f64::NEG_INFINITY, f64::max);
    let lo = w.iter().map(|b| b.low).fold(f64::INFINITY, f64::min);
    let last = bars.last().unwrap().close;
    if (hi - lo).abs() < f64::EPSILON {
        return 50.0;
    }
    ((last - lo) / (hi - lo) * 100.0).clamp(0.0, 100.0)
}

/// Two most recent swing highs and their gap in percent (0.0 when there is no
/// clear double top). A fractal high has `left`/`right` lower bars each side.
pub fn double_top_gap_pct(bars: &[Candle], swing: usize) -> f64 {
    if bars.len() < swing * 2 + 3 {
        return 0.0;
    }
    let mut highs: Vec<f64> = Vec::new();
    for i in swing..(bars.len() - swing) {
        let h = bars[i].high;
        let is_high = (1..=swing).all(|k| bars[i - k].high <= h && bars[i + k].high <= h);
        if is_high {
            highs.push(h);
        }
    }
    if highs.len() < 2 {
        return 0.0;
    }
    let a = highs[highs.len() - 1];
    let b = highs[highs.len() - 2];
    if a <= 0.0 || b <= 0.0 {
        return 0.0;
    }
    (a - b).abs() / a.max(b) * 100.0
}

/// Close-based 5m bearish structure: the most recent close broke the prior
/// `swing` fractal low. Returns true when a bearish BOS/CHoCH just printed.
pub fn bearish_5m(bars: &[Candle], swing: usize) -> bool {
    if bars.len() < swing * 2 + 3 {
        return false;
    }
    let mut last_low: Option<f64> = None;
    for i in swing..(bars.len() - swing) {
        let l = bars[i].low;
        let is_low = (1..=swing).all(|k| bars[i - k].low >= l && bars[i + k].low >= l);
        if is_low {
            last_low = Some(l);
        }
    }
    match last_low {
        Some(l) => bars.last().unwrap().close < l,
        None => false,
    }
}

/// Simplified short score (0-100) from the same signals the original short_rank
/// uses: 24h pump, premium zone, double-top tightness, distance from high,
/// ATR expansion and 5m bearish structure. Intentionally compact — the exact
/// original scoring lives in the hourly ranking routine, this is a faithful
/// proxy good enough to order the watch list.
pub fn short_score(
    pct_24h: f64,
    premium: f64,
    double_top_gap: f64,
    dist_from_high: f64,
    atr_expansion: f64,
    bear_5m: bool,
) -> i32 {
    let mut score = 0.0;
    // 24h pump: the bigger the pump, the more interest in shorting the top.
    if pct_24h >= 25.0 {
        score += 25.0;
    } else if pct_24h >= 15.0 {
        score += 20.0;
    } else if pct_24h >= 10.0 {
        score += 15.0;
    } else if pct_24h >= 5.0 {
        score += 8.0;
    }
    // Premium (near top of range) is where shorts want to enter.
    score += (premium / 100.0) * 25.0;
    // Tight double top = strong signal.
    if double_top_gap > 0.0 {
        if double_top_gap <= 0.5 {
            score += 20.0;
        } else if double_top_gap <= 1.5 {
            score += 14.0;
        } else if double_top_gap <= 3.0 {
            score += 8.0;
        } else {
            score += 3.0;
        }
    }
    // Closeness to the high: closer = better short entry.
    if dist_from_high <= 0.5 {
        score += 15.0;
    } else if dist_from_high <= 2.0 {
        score += 12.0;
    } else if dist_from_high <= 5.0 {
        score += 8.0;
    } else if dist_from_high <= 10.0 {
        score += 3.0;
    }
    // ATR expansion confirms the move has fuel.
    if atr_expansion >= 30.0 {
        score += 10.0;
    } else if atr_expansion >= 10.0 {
        score += 6.0;
    }
    if bear_5m {
        score += 5.0;
    }
    score.round().clamp(0.0, 100.0) as i32
}

/// Percentage drop from a baseline price. Negative when the price went up.
pub fn drop_pct(baseline: f64, current: f64) -> f64 {
    if baseline <= 0.0 {
        return 0.0;
    }
    (baseline - current) / baseline * 100.0
}

/// True when this row should fire: not yet alerted, has a usable baseline and
/// current price, and the drop meets or exceeds the threshold.
pub fn should_alert(alerted: bool, baseline: f64, current: f64, threshold: f64) -> bool {
    if alerted {
        return false;
    }
    if baseline <= 0.0 || current <= 0.0 {
        return false;
    }
    drop_pct(baseline, current) + 1e-12 >= threshold
}

/// The watch list is only valid inside its 1h window.
pub fn is_expired(now_epoch: f64, expires_epoch: f64) -> bool {
    now_epoch >= expires_epoch
}

/// Lark markdown body of the dump alert card. Matches the Python original.
pub fn feishu_card_body(
    symbol: &str,
    short_score: Option<i32>,
    baseline: f64,
    current: f64,
    drop_pct: f64,
    now_str: &str,
    threshold: f64,
) -> String {
    let link = format!("https://www.binance.com/zh-CN/futures/{symbol}");
    let score_txt = match short_score {
        Some(s) => format!("**做空分**：{s}\n"),
        None => String::new(),
    };
    let thr = if threshold.fract() == 0.0 {
        format!("{}", threshold as i64)
    } else {
        format!("{threshold}")
    };
    format!(
        "**[{symbol}]({link})** 相对监控基线暴跌 **{drop_pct:.2}%**\n\
         {score_txt}\
         **基线价**：{baseline}\n\
         **现价**：{current}\n\
         **跌幅**：{drop_pct:.2}%\n\
         **触发时间**：{now_str}（上海）\n\
         _阈值：距名单保存 1 小时内，相对基线跌 ≥{thr}% 且每币每轮名单只告警一次_",
    )
}

/// Card title, kept as a helper so it is testable.
pub fn feishu_card_title(now_str: &str) -> String {
    format!("暴跌告警 · {now_str}")
}

#[cfg(test)]
mod tests;
