use super::*;

fn c(ts: i64, o: f64, h: f64, l: f64, cl: f64) -> Candle {
    Candle { ts, open: o, high: h, low: l, close: cl }
}

#[test]
fn candidates_filters_leveraged_and_fiat_and_deliveries() {
    let t = vec![
        Ticker::new("BTCUSDT", 3.0, 1e9),
        Ticker::new("ETHUPUSDT", 50.0, 1e6),   // leveraged token
        Ticker::new("XUSD1", 30.0, 1e6),        // USD1 quoted
        Ticker::new("BTCUSDT_261225", 40.0, 1e6), // delivery
        Ticker::new("BTCEURUSDT", 20.0, 1e6),   // fiat composite base ends EUR
        Ticker::new("DOGEUSDT", -5.0, 1e6),     // loser, excluded
    ];
    let out = pick_candidates(&t, 10);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].symbol, "BTCUSDT");
}

#[test]
fn candidates_prefers_hot_ge5_sorted_desc() {
    let t = vec![
        Ticker::new("AUSDT", 2.0, 0.0),
        Ticker::new("BUSDT", 30.0, 0.0),
        Ticker::new("CUSDT", 7.0, 0.0),
        Ticker::new("DUSDT", 12.0, 0.0),
    ];
    let out = pick_candidates(&t, 2);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].symbol, "BUSDT");
    assert_eq!(out[1].symbol, "DUSDT");
}

#[test]
fn candidates_tops_up_when_hot_insufficient() {
    let t = vec![
        Ticker::new("AUSDT", 6.0, 0.0),
        Ticker::new("BUSDT", 3.0, 0.0),
        Ticker::new("CUSDT", 1.0, 0.0),
    ];
    let out = pick_candidates(&t, 3);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].symbol, "AUSDT");
}

#[test]
fn drop_pct_math() {
    assert!((drop_pct(100.0, 90.0) - 10.0).abs() < 1e-9);
    assert!((drop_pct(0.15616, 0.13896) - 11.0143).abs() < 1e-3);
    assert!(drop_pct(100.0, 110.0) < 0.0); // went up
    assert_eq!(drop_pct(0.0, 50.0), 0.0);
}

#[test]
fn should_alert_edge_trigger_once() {
    assert!(should_alert(false, 100.0, 89.0, 10.0)); // 11% drop, not alerted
    assert!(!should_alert(true, 100.0, 89.0, 10.0)); // already alerted
    assert!(!should_alert(false, 100.0, 95.0, 10.0)); // only 5%
    assert!(!should_alert(false, 0.0, 95.0, 10.0));   // bad baseline
    assert!(!should_alert(false, 100.0, 0.0, 10.0));  // bad current
}

#[test]
fn should_alert_threshold_boundary() {
    // exactly 10% drop triggers (>= with epsilon)
    assert!(should_alert(false, 100.0, 90.0, 10.0));
}

#[test]
fn expiry_window() {
    assert!(!is_expired(1000.0, 2000.0));
    assert!(is_expired(2000.0, 2000.0));
    assert!(is_expired(2001.0, 2000.0));
}

#[test]
fn premium_pct_range_position() {
    let bars = vec![c(0, 0.0, 100.0, 0.0, 90.0)];
    assert!((premium_pct(&bars, 10) - 90.0).abs() < 1e-9);
    let bars = vec![c(0, 0.0, 100.0, 0.0, 0.0)];
    assert!((premium_pct(&bars, 10) - 0.0).abs() < 1e-9);
    // flat range -> 50
    let bars = vec![c(0, 0.0, 50.0, 50.0, 50.0)];
    assert!((premium_pct(&bars, 10) - 50.0).abs() < 1e-9);
}

#[test]
fn atr_pct_basic() {
    // 20 bars each with range 10, last close 100 -> atr% = 10
    let bars: Vec<Candle> = (0..20).map(|i| c(i, 95.0, 105.0, 95.0, 100.0)).collect();
    let v = atr_pct(&bars, 14);
    assert!((v - 10.0).abs() < 1e-9, "got {v}");
    assert_eq!(atr_pct(&bars[..5], 14), 0.0); // not enough data
}

#[test]
fn double_top_gap_detects_two_peaks() {
    // build a series with two clear fractal highs of 100 and 99.5 -> gap ~0.5%
    let mut bars = Vec::new();
    let mut ts = 0;
    // rising to first peak
    for i in 0..5 {
        bars.push(c(ts, 0.0, 50.0 + i as f64 * 10.0, 0.0, 0.0));
        ts += 1;
    }
    bars.push(c(ts, 0.0, 100.0, 0.0, 0.0)); // first peak
    ts += 1;
    for i in 0..5 {
        bars.push(c(ts, 0.0, 90.0 - i as f64 * 5.0, 0.0, 0.0));
        ts += 1;
    }
    for i in 0..5 {
        bars.push(c(ts, 0.0, 70.0 + i as f64 * 6.0, 0.0, 0.0));
        ts += 1;
    }
    bars.push(c(ts, 0.0, 99.5, 0.0, 0.0)); // second peak
    ts += 1;
    for i in 0..5 {
        bars.push(c(ts, 0.0, 90.0 - i as f64 * 5.0, 0.0, 0.0));
        ts += 1;
    }
    let gap = double_top_gap_pct(&bars, 2);
    assert!(gap > 0.0 && gap < 1.0, "got {gap}");
}

#[test]
fn bearish_5m_detects_close_below_prior_low() {
    let mut bars = Vec::new();
    // fractal low at index 3 (low=10), then later close below it
    for i in 0..10 {
        let low = if i == 3 { 10.0 } else { 50.0 + i as f64 };
        bars.push(c(i, 60.0, 70.0, low, 60.0));
    }
    // final bar closes under 10
    bars.push(c(10, 60.0, 70.0, 5.0, 6.0));
    assert!(bearish_5m(&bars, 2));
    // without the break it is false
    let mut bars2 = bars.clone();
    let last = bars2.len() - 1;
    bars2[last].close = 60.0;
    bars2[last].low = 55.0;
    assert!(!bearish_5m(&bars2, 2));
}

#[test]
fn short_score_monotonic_with_pump() {
    let low = short_score(3.0, 50.0, 0.0, 20.0, 0.0, false);
    let high = short_score(30.0, 95.0, 0.3, 0.2, 40.0, true);
    assert!(high > low);
    assert!(high >= 60, "got {high}");
}

#[test]
fn short_score_clamped() {
    let s = short_score(100.0, 100.0, 0.1, 0.0, 100.0, true);
    assert!((0..=100).contains(&s));
}

#[test]
fn feishu_card_body_contents() {
    let body = feishu_card_body("ARIAUSDT", Some(75), 0.15616, 0.13896, 11.0143, "2026-11-06 09:25:52", 10.0);
    assert!(body.contains("ARIAUSDT"));
    assert!(body.contains("做空分**：75"));
    assert!(body.contains("11.01"));
    assert!(body.contains("基线价"));
    assert!(body.contains("≥10%"));
    assert!(body.contains("binance.com/zh-CN/futures/ARIAUSDT"));
    let title = feishu_card_title("2026-11-06 09:25:52");
    assert!(title.contains("暴跌告警"));
}

#[test]
fn feishu_card_body_without_score() {
    let body = feishu_card_body("XUSDT", None, 1.0, 0.8, 20.0, "t", 10.0);
    assert!(!body.contains("做空分"));
}

fn sample_row(sym: &str, score: i32, pct: f64) -> RankRow {
    RankRow {
        symbol: sym.into(),
        short_score: score,
        pct,
        mcap_rank: Some(100),
        premium: 80.0,
        peak_gap_pct: 0.3,
        dist_high_pct: 1.2,
        atr_expansion: 60.0,
        atr_pct: 3.5,
        bear_5m: true,
    }
}

#[test]
fn rank_card_title_and_body() {
    let rows = vec![
        sample_row("LYNUSDT", 93, 9.88),
        sample_row("BTWUSDT", 87, 11.15),
    ];
    let md = format_rank_md(&rows, "2026-11-06 10:00");
    assert!(md.contains("现在最该做空的是 [LYNUSDT]"));
    assert!(md.contains("做空分 **93**"));
    assert!(md.contains("## 做空分排序"));
    assert!(md.contains("1. **93分** [LYNUSDT]"));
    assert!(md.contains("2. **87分** [BTWUSDT]"));
    assert!(md.contains("市值#100"));
    assert!(md.contains("binance.com/zh-CN/futures/LYNUSDT"));
    assert!(md.contains("数据币安U本位 2026-11-06 10:00（上海）"));
    let title = rank_feishu_card_title("2026-11-06 10:00");
    assert_eq!(title, "做空优先级排序 · 2026-11-06 10:00");
}

#[test]
fn rank_card_empty_is_empty() {
    assert!(format_rank_md(&[], "t").is_empty());
}

#[test]
fn rank_card_low_score_note() {
    let rows = vec![sample_row("XUSDT", 30, 4.0)];
    let md = format_rank_md(&rows, "t");
    assert!(md.contains("不算很高"));
}

#[test]
fn plan_label_tiers() {
    assert_eq!(plan_label(60, 15.0, 0.4, true, true), "最优先摸顶空·可加仓");
    assert_eq!(plan_label(50, 15.0, 0.4, true, true), "摸顶做空");
    assert_eq!(plan_label(50, 15.0, 0.4, false, true), "顶部观察·等5m空");
    assert_eq!(plan_label(25, 6.0, 1.5, false, false), "观望");
    assert_eq!(plan_label(32, 12.0, 3.0, false, false), "弱续空");
}

#[test]
fn should_send_rank_once_per_hour() {
    assert!(should_send_rank(None, "2026-11-06-10"));
    assert!(should_send_rank(Some("2026-11-06-09"), "2026-11-06-10"));
    assert!(!should_send_rank(Some("2026-11-06-10"), "2026-11-06-10"));
}

#[test]
fn mcap_label_known_and_unknown() {
    assert_eq!(mcap_label(Some(55)), "市值#55");
    assert_eq!(mcap_label(None), "市值排名未知");
}
