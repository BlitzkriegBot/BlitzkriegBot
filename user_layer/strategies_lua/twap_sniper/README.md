# twap_sniper

Final-15s TWAP-lock sniper for 5m binary rounds. With `5–15` seconds left
(when 75%–92% of the 60s Chainlink settlement TWAP is already locked in),
buys any lagging ask inside `0.90–0.96` as a taker and holds to settlement.
One attempt per condition per round.

On 12.5 days of live-recorded 5m L2 orderbook archives (2026-11-11 through
2026-11-24, 5,527 settled BTC/ETH/SOL/XRP rounds, 10 shares/trade, official
`0.07 * p * (1 - p)` taker fee):

- ~15.4 trades/day; 193 trades total, 92.75% WR, PF 1.802, net +94.40,
  max DD 19.80 (Train PF 1.813, OOS PF 1.777; +300ms / +1¢ FOK stress:
  76 trades, PF 1.456, net +31.00).
- All 4 assets positive in both Train and OOS (BTC, ETH, SOL, XRP).
- Note: median best-ask depth in the final 15s is ~$25; keep per-order size
  modest (~$25–$35) to avoid sweeping thin books.

Enable explicitly (`DRY_RUN` stays default):

```sh
--enable-strategy twap_sniper
```

| tunable | default | meaning |
|---|---|---|
| `min_px` / `max_px` | 0.90 / 0.96 | inclusive entry band |
| `min_time_left` / `max_time_left` | 5 / 15 | seconds remaining |
| `maker_entry` | 0 | 0 = taker at ask (`late-taker`), 1 = post-only bid |
