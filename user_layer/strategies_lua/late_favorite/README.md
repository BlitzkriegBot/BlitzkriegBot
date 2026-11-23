# late_favorite

Near-settlement BTC 5m favorite filtered by Digital Black-Scholes fair probability edge,
conservative multi-horizon realized spot volatility `max(σ120, σ300, σ600)`, and 15s spot momentum.
Holds to settlement; one entry per condition per round.

The registry id is `late_favorite`. The loader does not require a `lua_`
prefix; the directory name must equal `manifest.name`.

## Out-of-Sample Full-Tape Validation (BTC 5m, 10 shares, `0.07*p*(1-p)` taker fee)

| Window | Dates (UTC) | Baseline (0.80-0.90) | New Defaults (0.75-0.86, e>=0.10) | Stress (+2s lag, +1c slip) |
|---|---|---|---|---|
| Train (40d) | 09-12..09-26 + 09-30..10-26 | 7,661 · 84.01% · PF 0.880 | 1,125 · 87.82% · PF 1.781 · +$852.20 | PF 1.458 |
| Val (10d) | 10-26..11-05 | 1,841 · 83.76% · PF 0.862 | 272 · 86.03% · PF 1.543 · +$162.10 | PF 1.414 |
| Seen (8d) | 11-05..11-13 | 1,511 · 84.71% · PF 0.913 | 243 · 84.36% · PF 1.349 · +$105.19 | 194 · 83.51% · PF 1.201 |
| **Unused Holdout (9d)** | **11-13..11-22** | 1,673 · 82.96% · PF 0.811 | **225 · 84.00% · PF 1.375 · +$106.06** | **169 · 84.62% · PF 1.354** |

Backtest results are not live guarantees; `DRY_RUN` stays `true` by default:

```sh
--enable-strategy late_favorite
```

| tunable | default | meaning |
|---|---|---|
| `btc_only` | 1 | BTC only (prevents correlated multi-asset round wipeouts) |
| `max_round_sec` | 300 | Max round duration in seconds (300 = 5m only; 0 = all) |
| `maker_entry` | 0 | 0 = taker at the ask (`late-taker`). 1 = post-only bid |
| `min_px` / `max_px` | 0.75 / 0.86 | inclusive entry price band |
| `min_time_left` / `max_time_left` | 20 / 120 | seconds remaining |
| `min_edge` | 0.10 | minimum `fair - (ask + taker_fee)` edge |
| `mom_window_sec` | 15 | spot momentum alignment lookback in seconds |
| `max_spot_age_ms` | 5000 | fail-closed spot freshness guard (ms) |

`bk_thesis_death` returns `false`. Timing and spot-momentum gates are waived down
to 20 seconds (`gate_exemptions.timing_min_time_left_sec = 20`). Details:
`docs/late-favorite.md` and `docs/reports/late-favorite-bs-edge-2026-11.md`.
