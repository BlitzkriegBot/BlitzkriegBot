# late_favorite

Buy the favourite side priced 0.94–0.97 while 45–60 seconds remain, then hold
it to settlement. One attempt per condition per round. Applies to every asset
the venue lists (BTC, ETH, SOL, XRP, …).

The registry id is `late_favorite`. The loader does not require a `lua_`
prefix; the directory name must equal `manifest.name`.

On 12.5 days of live-recorded 5m L2 orderbook archives (2026-11-11 through
2026-11-24, 5,527 settled BTC/ETH/SOL/XRP rounds), the prior 0.80–0.90 /
45–120s defaults lost money (3,546 trades, 83.76% WR, PF 0.886, net −556.50).
Tightening to 0.94–0.97 / 45–60s maintains ~114 trades/day while turning
positive both in-sample (995 trades, 96.08% WR, PF 1.097, net +36.20) and
out-of-sample (428 trades, 96.73% WR, PF 1.291, net +38.80; full window
1,423 trades, PF 1.148, net +75.00, max DD 50.80; +300ms / +1¢ FOK stress:
997 trades, PF 1.350, net +124.90). Those are backtest numbers. Live results
can differ. `DRY_RUN` stays the process default. The package is not in the
default enabled set. Enable it explicitly:

```sh
--enable-strategy late_favorite
```

| tunable | default | meaning |
|---|---|---|
| `maker_entry` | 0 | 0 = taker at the ask. 1 = post-only bid, cancelled once time left falls below `min_time_left` |
| `min_px` / `max_px` | 0.94 / 0.97 | inclusive band |
| `min_time_left` / `max_time_left` | 45 / 60 | seconds remaining |

`bk_thesis_death` returns false. The thesis is the redemption. An
intra-round bid drop is the variance the entry price already charges.
Timing and spot-momentum gates are waived down to 45 seconds so the
scanner's 180-second floor does not reject every order. Risk limits still
apply.
