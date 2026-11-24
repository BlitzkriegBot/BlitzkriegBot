/**
 * Regression check for the panel's balance accounting (`src/lib/balance.ts`,
 * `src/lib/trades.ts`).
 *
 * The properties pinned here:
 *
 *   - the view **passes ledger fields through**: `net` is the core's
 *     `cumulativeNetPnl` (the SSOT) and the cash ledger is reported as `cash` —
 *     never recomposed into a different figure;
 *   - there is **no equity arithmetic**. `AccountView` carries neither `equity`
 *     nor `opening_equity`, and `seeded_total` is 累计 Seed 入金 rather than the
 *     期初权益 that net profit is measured against, so `seed + net` was simply
 *     a wrong sum; `equity` is therefore always null (#179, R7.1 L5);
 *   - fees are reported as a cost, and gross profit is `net + fees` — the figure
 *     the cost is charged against, on the trade-fold basis;
 *   - closed-trade rows collapse only when they describe the SAME trade: `hft-N`
 *     is a per-boot counter, so 267 distinct trades can carry 240 ids and keying
 *     the dedupe on the id silently deletes real trades from every row-derived
 *     aggregate (the equity curve ended $9.24 short of the summary above it);
 *
 * Retired by #179 (kept here as a record, not as live assertions):
 *
 *   - 「the headline is 本金 ＋ 净利润, not the core cash ledger」 — the premise
 *     was that seed was an opening equity. It is not, so the composed headline
 *     was a number the kernel never published.
 *   - 「a reset ledger is flagged identityBroken」 — the flag policed
 *     `cash == seed + net`, which does not hold whenever
 *     `opening_equity ≠ seeded_total`; `polysimulator`'s $2.31 gap was the same
 *     defect. Both the flag and the equation behind it are gone.
 *   - 「an unknown principal yields no equity」 — superseded: equity is null
 *     unconditionally, and that invariant is pinned by `check:money`
 *     (`frontend-money-ssot.check.mjs`) for every seed/net combination.
 *
 *   cd ui/webapp/webui && npm run check:balance
 */
import assert from 'node:assert/strict'

let failures = 0
const check = (label, fn) => {
  try {
    fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

const { balanceView } = await import('../src/lib/balance.ts')
const { dedupeTrades, tradeIdentity } = await import('../src/lib/trades.ts')

const balance = (over) => ({ balance: 0, reserved: 0, available: 0, seed: null, ...over })

console.log('balance basis — ledger fields pass through, no equity arithmetic')

check('net is the core ledger SSOT, passed through untouched', () => {
  // cash 1073.29, seed 1000, net 39.44 — the figure the kernel published. The
  // panel reports it as-is; it does not re-derive it from cash − seed (which is
  // what produced "净利 = 全部余额" on LIVE once the seed was missing).
  const v = balanceView(balance({ balance: 1073.29, available: 1073.29, seed: 1000 }), 39.44, 34.96)
  assert.equal(v.net, 39.44)
  assert.equal(v.cash, 1073.29, 'the ledger cash is reported as cash, nothing else')
})

check('the cash ledger is never promoted to equity', () => {
  // Measured on 2026-09-16: seed 1000, core cash 1002.06 (the ledger had been
  // reseeded at that boot and covered only that run), all-time net 48.21. The
  // cash figure is 本金 ＋ 本次会话净利 — a strict subset of the trade log — so
  // printing it under an equity label reported the wrong amount.
  const v = balanceView(balance({ balance: 1002.06, available: 1002.06, seed: 1000 }), 48.21, 38.69)
  assert.equal(v.equity, null, 'no equity on the wire → 「—」, never the cash figure')
  assert.equal(Number(v.cash.toFixed(2)), 1002.06)
  assert.equal(Number(v.net.toFixed(2)), 48.21)
})

check('equity is null however the ledger moves', () => {
  // A different ledger cannot change an equity the panel does not have.
  const a = balanceView(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44, 0)
  const b = balanceView(balance({ balance: 900, available: 900, seed: 1000 }), 39.44, 0)
  assert.equal(a.equity, null)
  assert.equal(b.equity, null)
})

check('a fee is an expense: it reduces net profit, never the principal', () => {
  // Same trade with and without fees. The net profit absorbs the cost; gross is
  // the pre-cost figure fees are charged against (trade-fold basis, not ledger).
  const withFees = balanceView(balance({ balance: 1039.44, available: 1039.44, seed: 1000 }), 39.44, 34.96)
  assert.equal(Number(withFees.gross.toFixed(2)), 74.40, 'gross = net + fees')
  assert.equal(Number(withFees.net.toFixed(2)), 39.44, 'net is already after fees')
  assert.ok(withFees.gross > withFees.net, 'the expense is deducted from gross, not added')
})

check('reserved cash is reported, never folded into anything', () => {
  // 1050 cash with 10.56 tied up in resting buys is 1039.44 of free cash. The
  // reservation is surfaced on its own; it is not profit and not equity.
  const v = balanceView(balance({ balance: 1050, reserved: 10.56, available: 1039.44, seed: 1000 }), 39.44, 0)
  assert.equal(v.reserved, 10.56)
  assert.equal(v.available, 1039.44)
  assert.equal(v.equity, null)
})

check('a missing balance object degrades to zeros, not NaN', () => {
  const v = balanceView(undefined, 0, 0)
  assert.deepEqual([v.cash, v.reserved, v.available], [0, 0, 0])
  assert.equal(v.equity, null)
  assert.ok(Number.isFinite(v.gross), 'gross must stay a number')
})

check('a loss keeps its sign through net (equity stays unknown)', () => {
  const v = balanceView(balance({ balance: 900, available: 900, seed: 1000 }), -50, 0)
  assert.equal(v.net, -50)
  assert.equal(v.equity, null, 'a loss does not make an unavailable equity computable')
  assert.equal(Number(v.cash.toFixed(6)), 900)
})

console.log('\nclosed-trade dedupe')

check('distinct trades that share a per-boot counter id are both kept', () => {
  // The live log: 267 rows, 240 distinct ids, `hft-1` standing for six different
  // trades. Keying on the id deleted 27 real trades and $9.24 of net PnL, so the
  // equity curve ended at $38.72 under a header reading $47.96.
  const rows = [
    { id: 'hft-1', asset: 'BTC', entryTime: 100, exitTime: 200, netPnlUsd: -0.75 },
    { id: 'hft-1', asset: 'ETH', entryTime: 300, exitTime: 400, netPnlUsd: 0.85 },
    { id: 'hft-1', asset: 'SOL', entryTime: 500, exitTime: 600, netPnlUsd: 3.18 },
  ]
  const out = dedupeTrades(rows)
  assert.equal(out.length, 3, 'three distinct trades, one reused counter id')
  const net = Number(out.reduce((a, r) => a + r.netPnlUsd, 0).toFixed(2))
  assert.equal(net, 3.28, 'every trade counts toward the net')
})

check('a genuine re-append of the SAME trade still collapses', () => {
  // Same asset and same entry/exit timestamps: one trade written twice.
  const rows = [
    { id: 'hft-7', asset: 'BTC', entryTime: 100, exitTime: 200, netPnlUsd: 5 },
    { id: 'hft-7', asset: 'BTC', entryTime: 100, exitTime: 200, netPnlUsd: -1 },
  ]
  const out = dedupeTrades(rows)
  assert.equal(out.length, 1, 'the same trade is not counted twice')
  assert.equal(out[0].netPnlUsd, -1, 'the last write wins')
})

check('the identity key is timestamps+asset, not the counter id', () => {
  assert.equal(
    tradeIdentity({ id: 'hft-1', asset: 'BTC', entryTime: 1, exitTime: 2 }),
    tradeIdentity({ id: 'hft-9', asset: 'BTC', entryTime: 1, exitTime: 2 }),
    'the same trade under different counter values is one trade',
  )
  assert.notEqual(
    tradeIdentity({ id: 'hft-1', asset: 'BTC', entryTime: 1, exitTime: 2 }),
    tradeIdentity({ id: 'hft-1', asset: 'BTC', entryTime: 1, exitTime: 3 }),
    'a different exit is a different trade even at the same id',
  )
})

check('rows without timestamps fall back to the counter, never all collapse', () => {
  // An older core omits entryTime/exitTime; the counter is the only key left, so
  // two rows under one id collapse — a possible over-collapse, but the log's rows
  // are genuinely indistinguishable at that point.
  const rows = [
    { id: 'hft-1', asset: 'BTC', netPnlUsd: 1 },
    { id: 'hft-2', asset: 'ETH', netPnlUsd: 2 },
  ]
  assert.equal(dedupeTrades(rows).length, 2, 'distinct ids stay distinct')
  assert.equal(tradeIdentity(rows[0]), 'i:hft-1', 'falls back to the counter')
  const sameId = [{ id: 'hft-1', asset: 'BTC' }, { id: 'hft-1', asset: 'BTC' }]
  assert.equal(dedupeTrades(sameId).length, 1)
})

check('insertion order is preserved so the curve stays chronological', () => {
  const rows = [
    { id: 'hft-2', asset: 'A', entryTime: 2, exitTime: 3 },
    { id: 'hft-1', asset: 'B', entryTime: 1, exitTime: 2 },
    { id: 'hft-1', asset: 'C', entryTime: 5, exitTime: 6 },
  ]
  assert.deepEqual(dedupeTrades(rows).map((r) => r.asset), ['A', 'B', 'C'])
})

check('the deduped list no longer under-counts the summary', () => {
  // The live shape: 267 rows, 240 unique ids, all rows distinct trades. After
  // the fix the deduped sum must equal the raw sum — which is what the kernel's
  // own summary reports, so the curve and the header agree.
  const rows = []
  for (let i = 1; i <= 240; i++) {
    rows.push({ id: `hft-${i}`, asset: 'BTC', entryTime: i * 10, exitTime: i * 10 + 5, netPnlUsd: 0.1 })
  }
  for (let i = 1; i <= 27; i++) {
    rows.push({ id: `hft-${i}`, asset: 'ETH', entryTime: 1e6 + i, exitTime: 1e6 + i + 5, netPnlUsd: 0.2 })
  }
  const sum = (xs) => Number(xs.reduce((a, r) => a + r.netPnlUsd, 0).toFixed(6))
  const deduped = dedupeTrades(rows)
  assert.equal(rows.length, 267)
  assert.equal(deduped.length, 267, 'no distinct trade is dropped')
  assert.equal(sum(deduped), sum(rows), 'the curve total now matches the summary')
})

check('an empty or absent list stays empty', () => {
  assert.deepEqual(dedupeTrades([]), [])
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
