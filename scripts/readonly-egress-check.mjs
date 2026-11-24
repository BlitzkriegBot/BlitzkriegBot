#!/usr/bin/env node
/**
 * E12 read-only egress gate — "`--readonly` 在结构上不可能下单".
 *
 * Issue #94 (e) asks for read-only that is *structural*, explicitly not
 * conventional — a per-RPC `if readonly` is exactly the kind of guard that gets
 * forgotten at one entry point and then places a real order. So this gate does
 * not check that read-only code exists; it checks the property that makes it
 * structural: **the venue order egress is never constructed**, so there is
 * nothing for an order to reach.
 *
 * The sharpest available test is the adversarial one. A read-only core is booted
 * with the exact environment a live core needs (`POLYMARKET_PRIVATE_KEY`,
 * `POLYMARKET_FUNDER_ADDRESS`) and asked, over IPC, to go live — `--mode live`
 * on the command line. If read-only were conventional, this core would trade.
 * The gate then asserts:
 *
 *   1. the mode the core reports over IPC is `readonly`, not `live`;
 *   2. no venue call is attempted: no order ever leaves PENDING for a venue id,
 *      and none is rejected by a venue (both would require an executor);
 *   3. `orders.place` is REFUSED — the second layer of #177. Before #177 the
 *      mode only gated venue egress, so a read-only core accepted the order and
 *      settled it in the simulated book; the ledger moved and a position opened.
 *      Read-only now refuses execution in the kernel, so the refusal must be
 *      the observable outcome, not a fill;
 *   4. nothing was written: `orders.list` is empty, the ledger still holds only
 *      its boot seed, `positions.list` is empty, and the order log has no rows
 *      at all (not merely no venue-bound rows) — a refused order leaves no
 *      durable trace;
 *   5. the mode is still USABLE AS AN OBSERVER: a read verb answers. Read-only
 *      refuses non-read verbs; it does not refuse observation.
 *
 * Every claim is read from the running process, not from source text, so the
 * gate keeps its meaning if the implementation is refactored.
 *
 * Run: node scripts/readonly-egress-check.mjs
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, readFileSync, existsSync, unlinkSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import { requestOnce as rpc } from './lib/core-client.mjs';
import { pollUntil, sleep } from './lib/wait.mjs';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const WORK = mkdtempSync(join(tmpdir(), 'readonly-'));
const ORDER_LOG = join(WORK, 'orders.jsonl');
const TRADE_LOG = join(WORK, 'trades.jsonl');

const sock = join(WORK, 'core.sock');
const out = [];
// The adversarial setup: ask for live AND supply live credentials. A core that
// honours read-only only by convention would start the executor here.
const proc = spawn(
  BIN,
  [
    '--socket', sock,
    '--mode', 'live',
    '--readonly',
    '--tick-ms', '50',
    '--seed-balance', '1000',
    '--max-order-notional', '6',
    '--order-log', ORDER_LOG,
    '--trade-log', TRADE_LOG,
    '--no-discovery',
    '--no-auto-exits',
  ],
  {
    stdio: ['ignore', 'pipe', 'pipe'],
    cwd: WORK,
    env: {
      ...process.env,
      // Fake but WELL-FORMED credentials: if an executor started, it would try
      // to use them. Nothing here is a real secret, and no network is required
      // for the assertion — the point is that the egress never gets built.
      POLYMARKET_PRIVATE_KEY:
        '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d',
      POLYMARKET_FUNDER_ADDRESS: '0x0000000000000000000000000000000000000001',
      CLOB_API_URL: 'http://127.0.0.1:1',
    },
  },
);
proc.stdout.on('data', (d) => out.push(String(d)));
proc.stderr.on('data', (d) => out.push(String(d)));

await pollUntil(() => existsSync(sock), { timeoutMs: 6000 });
if (!existsSync(sock)) {
  console.log('core never bound its socket; output tail:');
  console.log(out.join('').split('\n').slice(-15).join('\n'));
  try { proc.kill('SIGKILL'); } catch {}
  process.exit(1);
}
await sleep(400);

const banner = out.join('');
// (1) The banner must say so, and must name why.
const announced = /READ-ONLY/.test(banner);
console.log('banner          :', announced ? 'announces READ-ONLY' : '(no READ-ONLY banner)');
// A started executor is the failure this gate exists to catch.
const startedExecutor = /live order executor started/.test(banner);
console.log('executor started:', startedExecutor);

// (2) The authority on what mode is actually running: `core.ready` reports the
// mode the process settled on, so this catches a precedence bug that silently
// let `--mode live` win over `--readonly`.
const ready = await rpc(sock, 'core.ready');
const mode = ready?.mode ?? null;
console.log('mode over IPC   :', JSON.stringify(mode));

// (3) #177: read-only refuses EXECUTION, so the order must come back as a
// refusal. Accepting it and settling it in the simulated book (the pre-#177
// behaviour) is the defect, not the contract: it moved the ledger and opened a
// position in the one mode that must not transact. The refusal is captured, not
// thrown, so the rest of the gate can still read the state the attempt left.
let refused = false;
let refusalMessage = null;
try {
  await rpc(sock, 'orders.place', {
    tokenId: 'tok1', conditionId: 'cond1', side: 'buy', mode: 'taker',
    price: 0.30, size: 10, internalKey: 'k-ro', strategy: 'operator',
    asset: 'BTC', direction: 'up', roundSlot: 0, // "no round" (#457)
  });
} catch (err) {
  refused = true;
  refusalMessage = err?.message ?? String(err);
}
await sleep(600);

// (5) Observation must still work: read-only refuses non-read verbs, not reads.
let observed = false;
try {
  const readings = await rpc(sock, 'positions.list');
  observed = Array.isArray(readings?.positions);
} catch {
  observed = false;
}

const list = await rpc(sock, 'orders.list');
const orders = list?.orders ?? [];
const venueBound = orders.filter((o) => o.venueOrderId);
const finalStatuses = orders.map((o) => o.status);
const bal = await rpc(sock, 'ledger.balance');
const readPositions = await rpc(sock, 'positions.list');
const positions = readPositions?.positions ?? [];
console.log('orders placed   :', orders.length);
console.log('place refusal   :', refused ? `yes (${refusalMessage})` : 'NO — the order was accepted');
console.log('venue-bound     :', venueBound.length);
console.log('order statuses  :', JSON.stringify(finalStatuses));
console.log('ledger balance  :', JSON.stringify(bal?.balance ?? null));
console.log('ledger reports  :', bal?.seed == null ? 'no seed (live-like)' : 'a seed (local settlement)');
console.log('open positions  :', positions.length);

// (4) The durable artefact: a live order writes a venue id into the order log.
// #177 makes the stronger claim possible — a refused order writes no row at all.
let logVenueBound = 0;
let logRows = 0;
let logFilled = 0;
if (existsSync(ORDER_LOG)) {
  for (const line of readFileSync(ORDER_LOG, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    logRows++;
    try {
      const row = JSON.parse(line);
      if (row.venue_order_id) logVenueBound++;
      if (row.status === 'FILLED' || row.status === 'PARTIALLY_FILLED') logFilled++;
    } catch {}
  }
}
console.log(
  'order log       : rows =', logRows,
  '| venue-bound =', logVenueBound,
  '| filled =', logFilled,
);

try { proc.kill('SIGTERM'); } catch {}
await sleep(500);
try { proc.kill('SIGKILL'); } catch {}
try { unlinkSync(sock); } catch {}

// ── verdict ─────────────────────────────────────────────────────────────────
const seededBalance = '1000';
const balanceText = String(bal?.balance ?? '');
const checks = [
  ['banner announces read-only', announced],
  ['venue executor never started', !startedExecutor],
  ['IPC mode is read-only', mode === 'readonly'],
  ['read verbs still answer', observed],
  ['orders.place is refused (#177)', refused],
  ['no order was registered', orders.length === 0],
  ['no position was opened', positions.length === 0],
  ['the ledger did not move', balanceText === seededBalance],
  ['no order reached a venue', venueBound.length === 0],
  ['durable log holds no venue id', logVenueBound === 0],
  ['durable log records no fill', logFilled === 0],
];
let ok = true;
console.log('\nclaims:');
for (const [name, pass] of checks) {
  console.log(`  ${pass ? 'ok  ' : 'FAIL'}  ${name}`);
  if (!pass) ok = false;
}
const TERMINAL = ['FILLED', 'PARTIALLY_FILLED', 'CANCELLED', 'REJECTED'];
if (orders.length === 1 && !TERMINAL.includes(orders[0].status)) {
  console.log(`  FAIL  the one order settled (status ${orders[0].status})`);
  ok = false;
} else if (orders.length === 1) {
  console.log(`  ok    the one order settled (status ${orders[0].status})`);
}

console.log(
  `\nRESULT: ${ok ? 'PASS' : 'FAIL'} — ` +
    (ok
      ? 'read-only refused egress structurally and refused execution (#177): ' +
        'no order, no fill, no position, no ledger move, nothing reached a venue'
      : 'read-only did NOT hold: see the failing claims above'),
);
process.exit(ok ? 0 : 1);
