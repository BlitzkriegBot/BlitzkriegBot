# fresh_breakout

Mid-round fresh breakout favorite for 5m binary rounds. Records any leg that
traded at or below `neutral_cap` (default `0.55`) earlier in the round; with
`45–90` seconds left, buys that leg when it breaks into `0.94–0.97` and holds
to settlement. One attempt per condition per round.

On 12.5 days of live-recorded 5m L2 orderbook archives (2026-11-11 through
2026-11-24, 5,527 settled BTC/ETH/SOL/XRP rounds, 10 shares/trade, official
`0.07 * p * (1 - p)` taker fee):

- ~136 trades/day; Train 1,187 trades, 96.29% WR, PF 1.236, net +99.30;
  OOS 516 trades, 95.93% WR, PF 1.111, net +22.20 (+300ms / +1¢ FOK stress:
  PF 1.102, net +19.00).

Enable explicitly (`DRY_RUN` stays default):

```sh
--enable-strategy fresh_breakout
```

| tunable | default | meaning |
|---|---|---|
| `neutral_cap` | 0.55 | leg must have traded `<= neutral_cap` earlier in the round |
| `min_px` / `max_px` | 0.94 / 0.97 | inclusive breakout entry band |
| `min_time_left` / `max_time_left` | 45 / 90 | seconds remaining |
| `maker_entry` | 0 | 0 = taker at ask (`late-taker`), 1 = post-only bid |
