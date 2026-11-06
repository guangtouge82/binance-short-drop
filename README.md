# short_drop_monitor（做空榜 + 暴跌10%监控 · VPS 独立服务）

把原先作为 Grok 例行任务的「做空榜暴跌10%监控」和小时「做空优先级排序」
飞书推送，重写成一个常驻 VPS 的 Rust 服务，不再消耗 Grok 用量。

## 它做什么
1. 每小时（以及在服务启动、名单过期时）从 Binance USD-M 合约公开接口
   自包含地生成一份「做空榜」：
   - 取 24 小时涨幅榜候选（默认前 30，剔除杠杆代币 / 交割合约 / 法币计价）；
   - 对每个候选用一个简化版 SMC 做空分打分（24h 涨幅、Premium 区间、
     双顶紧密度、距高点距离、ATR 扩张、5m 空头结构）；
   - 用 CoinGecko 市值榜过滤掉股票 / 杠杆 ETF 代币（TZA、UVXY、SQQQ…），
     取前 15 组成监控名单，并记录此刻的价格作为「基线」。
   - CoinGecko 不可用时退回到一个内置的股票/ETF 黑名单兜底；
     若因此得到空名单，则保留上一份有效名单，不会把监控清空。
2. 名单有效 1 小时。期间每分钟检查一次现价，任何币相对基线跌 ≥10% 就
   触发一次飞书卡片告警（每币每轮名单只报一次）。告警规则与原版
   `dump_watch.py` 完全一致。
3. 每次小时刷新名单后，可选推送一张「做空优先级排序」飞书红卡（版式对齐
   原版 `short_rank.py` 的 `format_md` / `feishu_card`）。同一上海小时只推
   一次；重启不会在同一小时立刻重发（`last_rank_push_hour.txt`）。
   打分是简化版 SMC 代理分，字段比原版少（无独立顶显/5m 细项分），
   但标题、排序列表、市值标签、规则脚注保持一致。

## 纯函数与测试
所有核心逻辑都在 `src/lib.rs`（候选筛选、做空分、跌幅计算、触发/去重
判断、过期判断、暴跌卡片与做空榜卡片文案、小时去重）和 `src/mcap.rs`
（市值过滤、base 名解析、非加密黑名单）里，均为无 IO 纯函数，带单元测试。

运行测试：
    cargo test

## 部署（VPS root@178.128.63.185）
- 目录：`/opt/monitors/short_drop/`
- 二进制：`short_drop_monitor`（在 box 用 `cargo build --release` 编译后传入，
  绝不在 VPS 上编译——VPS 仅 458MB 内存）
- 配置：`short_drop.env`（mode 600），包含
  - `PUSH_FEISHU=0|1`  暴跌告警：0=影子只记日志，1=真正推飞书
  - `PUSH_RANK_FEISHU=0|1`  小时做空榜：0=影子只记日志（默认），1=真正推飞书
  - `FEISHU_WEBHOOK_URL=...`  飞书群机器人 webhook（与原 bot 同一 webhook；保密，勿入库）
  - `SHORT_DROP_DIR=/opt/monitors/short_drop`
- 服务：`short-drop-monitor.service`（`Restart=always`，`MemoryMax=120M`）

切换真实推送：
    编辑 short_drop.env：
      - 暴跌告警：PUSH_FEISHU=1
      - 小时做空榜：PUSH_RANK_FEISHU=1
    systemctl restart short-drop-monitor.service

## 状态与日志
- `watchlist.json`：当前名单、各币基线、已告警标记（原子写）
- `mcap_cache.json`：CoinGecko 市值榜缓存，6 小时 TTL
- `last_rank_push_hour.txt`：上次做空榜推送的上海小时键（YYYY-MM-DD-HH）
- `monitor.log`：运行日志，按天滚动，保留 7 天，单文件到 20MiB 提前切走；
  时间戳均为 Asia/Shanghai。影子模式下榜单正文以 `SHADOW rank |` 前缀落盘。

## 数据源
- Binance USD-M 合约公开接口（fapi.binance.com，备用 www.binance.com），
  多 host 轮换 + 重试，无需 API key。
- CoinGecko `/coins/markets`（仅用于过滤非加密代币；用 curl 子进程抓取，
  因为 Cloudflare 会挑战 ureq/rustls 的 TLS 指纹）。
