# consensus_favorite

Multi-asset aligned-direction favorite for 5m binary rounds. Enters a leg
priced `0.94–0.97` with `30–90` seconds left only when at least `min_consensus`
(default `4`) assets in the same round agree on the same direction at
`>= peer_min_px` (default `0.85`), then holds to settlement. One attempt per
condition per round.

On 12.5 days of live-recorded 5m L2 orderbook archives (2026-11-11 through
2026-11-24, 5,527 settled BTC/ETH/SOL/XRP rounds, 10 shares/trade, official
`0.07 * p * (1 - p)` taker fee):

- Default balanced high-frequency (`0.94–0.97`, `30–90s`, `4` coins `>= 0.85`):
  ~104 trades/day; Train 909 trades, 96.81% WR, PF 1.311, net +86.60; OOS 385
  trades, 97.92% WR, PF 2.060, net +81.20 (+300ms / +1¢ FOK stress: PF 2.081,
  net +71.30).
- Selective preset (`max_time_left=60`): ~50 trades/day; Train PF 1.789; OOS
  186 trades, 99.46% WR, PF 7.909, net +65.20 (+300ms / +1¢ stress: PF 5.862).
- Ultra-high-frequency preset (`min_px=0.90`, `min_time_left=45`,
  `max_time_left=120`): ~155 trades/day; Train PF 1.271; OOS 551 trades,
  95.64% WR, PF 1.322, net +72.50 (+300ms / +1¢ stress: PF 1.339).

Enable explicitly (`DRY_RUN` stays default):

```sh
--enable-strategy consensus_favorite
```

| tunable | default | meaning |
|---|---|---|
| `min_consensus` | 4 | minimum assets agreeing on the same direction |
| `peer_min_px` | 0.85 | minimum ask/bid on each peer to count as aligned |
| `min_px` / `max_px` | 0.94 / 0.97 | inclusive entry band |
| `min_time_left` / `max_time_left` | 30 / 90 | seconds remaining |
| `maker_entry` | 0 | 0 = taker at ask (`late-taker`), 1 = post-only bid |
