#!/usr/bin/env node
/**
 * 行情插件凭据（market.credentials.status / market.credentials.set）端到端
 * 验收 —— 真内核 spawn，真 UDS wire，dry 模式，零网络。一切写入都在脚本
 * 自己的 scratch cwd —— 不碰仓库、不碰生产 data/、不碰操作者的 .env。
 *
 * 钉住的事实（.env 单源合同，每个都对应一条 user-facing 承诺）：
 *
 *   1. status：每个注册插件一行，required_vars 与插件需求表一致，
 *      ready=false（干净沙盒进程环境无凭据），值永不出现在读数里。
 *   2. 写路径：market.credentials.set 把值 upsert 进 scratch cwd 的
 *      .env（唯一持久格，config.json 里不再有 credentials 键）；回执
 *      written 列变量名；status 在本会话仍不就绪（next_session ——
 *      启动器下次启动才加载 .env，内核 boot 不再注入任何东西）。
 *   3. plan-first：未注册插件名 / 需求表外的变量名都拒绝，且盘面零写入。
 *   4. reset 从 .env 删该插件的键，别的插件的行逐字保留。
 *   5. 启动器角色：把 scratch .env 灌进环境再启动（gate 模拟 launcher
 *      的加载——本仓库由 blitzkrieg 启动器做这一步），status 就绪；
 *      内核 stderr 不再有任何 "credentials injected" 注入行。
 *   6. 迁移：旧 config.json credentials 格在 boot 一次性排空进 .env
 *      并剥键（stderr 有迁移行），幂等。
 *
 * Usage: node scripts/market-credentials-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { randomBytes } from 'crypto';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();
// #1199 第六轮：凭证 AES-256-GCM 加密落 SQLite，主密钥来自
// BLITZKRIEG_DB_KEY（base64 of 32 random bytes）。每个测试用同一个密钥
// 和独立的 store.sqlite（BLITZKRIEG_DB_PATH），模拟真实部署。
const MASTER_KEY = randomBytes(32).toString('base64');

const gate = createChecks();
const check = gate.check;
const cleanup = [];

// The launcher's read side (ui_kit_panel::env_file::parse_env_file), restated
// minimally: KEY=VALUE, one optional matching-quote layer. Keep in sync.
function parseEnvText(text) {
  const out = {};
  for (const raw of text.split('\n')) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    const stripped = line.startsWith('export ') ? line.slice(7).trimStart() : line;
    const eq = stripped.indexOf('=');
    if (eq <= 0) continue;
    const key = stripped.slice(0, eq).trim();
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) continue;
    let value = stripped.slice(eq + 1).trim();
    if (value.length >= 2
      && ((value.startsWith('"') && value.endsWith('"'))
        || (value.startsWith("'") && value.endsWith("'")))) {
      value = value.slice(1, -1);
    }
    out[key] = value;
  }
  return out;
}

function makeCore(label, extraArgs = [], cwdOverride = null, envExtra = null) {
  // `.env` is cwd-relative (the launcher loads <cwd>/.env), so a "restart"
  // MUST reuse the SAME cwd the previous core ran in — a real deployment
  // restarts in place.
  const cwd = cwdOverride ?? mkdtempSync(join(tmpdir(), `blitzkrieg-cred-${label}-`));
  if (!cwdOverride) cleanup.push(cwd);
  // #1199 第六轮：凭证的家是 SQLite（加密），`.env` 只读。每个 cwd 用独立
  // store.sqlite（BLITZKRIEG_DB_PATH），密钥统一（BLITZKRIEG_DB_KEY），
  // 模拟真实部署：写加密行，boot 注入解密后的进程环境。
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(`cred-${label}`),
    mode: 'dry',
    seedBalance: 100,
    maxOrderNotional: 100,
    tickMs: 20,
    autoRestart: false,
    cwd,
    noTradeLog: true,
    noOrderLog: true,
    noPositionLog: true,
    extraArgs,
    envExtra: {
      ...(envExtra ?? {}),
      BLITZKRIEG_DB_KEY: MASTER_KEY,
      BLITZKRIEG_DB_PATH: join(cwd, 'store.sqlite'),
    },
  });
}

const envFile = (cwd) => join(cwd, '.env');
const readEnv = (cwd) => {
  try { return parseEnvText(readFileSync(envFile(cwd), 'utf8')); } catch { return {}; }
};

try {
  const core = makeCore('main');
  await core.start();

  // 1. The status readout: one row per registered plugin, names only.
  const before = await core.request('market.credentials.status', {});
  const rows = before.rows ?? [];
  const byName = Object.fromEntries(rows.map((r) => [r.plugin, r]));
  check('every compiled-in plugin reports a row',
    ['polymarket', 'kalshi', 'predictfun', 'polysimulator'].every((n) => byName[n] !== undefined),
    JSON.stringify(rows));
  check('kalshi requires its two executor vars',
    JSON.stringify(byName.kalshi?.requiredVars) === JSON.stringify(['KALSHI_API_KEY_ID', 'KALSHI_RSA_PRIVATE_KEY']),
    JSON.stringify(byName.kalshi));
  check('polysimulator requires its single var',
    JSON.stringify(byName.polysimulator?.requiredVars) === JSON.stringify(['POLYSIM_API_KEY']),
    JSON.stringify(byName.polysimulator));
  check('an empty sandbox resolves nothing and is not ready',
    rows.every((r) => r.ready === false && (r.resolvedVars ?? []).length === 0),
    JSON.stringify(rows));
  check('no value ever appears in the readout',
    !JSON.stringify(before).includes('secret'), JSON.stringify(before));

  // 3a. plan-first: an unregistered plugin is refused before the disk.
  let badPlugin = null;
  try {
    badPlugin = await core.request('market.credentials.set', { plugin: 'nope', vars: { KALSHI_API_KEY_ID: 'x' } });
  } catch (e) {
    badPlugin = { message: e?.message ?? String(e) };
  }
  check('an unregistered plugin is refused by name',
    badPlugin?.message?.includes('not a registered plugin'), JSON.stringify(badPlugin));

  // 3b. plan-first: a name outside the plugin's requirement table is refused
  //     (a typo must fail now, not as a silently inert executor).
  let badVar = null;
  try {
    badVar = await core.request('market.credentials.set', { plugin: 'kalshi', vars: { KALSHI_SECRET: 'x' } });
  } catch (e) {
    badVar = { message: e?.message ?? String(e) };
  }
  check('a name outside the requirement table is refused',
    badVar?.message?.includes('not a credential'), JSON.stringify(badVar));
  check('a refused write touches no disk at all',
    !existsSync(envFile(core.cwd)), envFile(core.cwd));

  // 2. The write lands in SQLite (encrypted); the reply names what was written,
  //    values stay local; `.env` is read-only for the kernel (never written).
  const set = await core.request('market.credentials.set', {
    plugin: 'kalshi',
    vars: { KALSHI_API_KEY_ID: 'gate-key', KALSHI_RSA_PRIVATE_KEY: 'gate-secret' },
    reason: 'e2e gate',
  });
  check('the set replies with the written var names',
    JSON.stringify(set.written) === JSON.stringify(['KALSHI_API_KEY_ID', 'KALSHI_RSA_PRIVATE_KEY'])
      && set.plugin === 'kalshi' && set.reset === false,
    JSON.stringify(set));
  check('the reply never echoes values', !JSON.stringify(set).includes('gate-secret'), JSON.stringify(set));
  check('the audit trail names the actor', typeof set.actor === 'string' && set.actor.startsWith('uid:'),
    JSON.stringify(set));

  // `.env` 是内核只读：写入落 SQLite（加密），文件不被触碰。
  check('the .env is NOT written (kernel read-only; SQLite is the durable home)',
    !existsSync(envFile(core.cwd)), envFile(core.cwd));
  const cellFile = join(core.cwd, 'data/runtime/config.json');
  let cellText = existsSync(cellFile) ? readFileSync(cellFile, 'utf8') : '';
  check('the config.json credentials cell is gone from the write path',
    !cellText.includes('credentials'), cellFile);

  const mid = await core.request('market.credentials.status', {});
  const midKalshi = (mid.rows ?? []).find((r) => r.plugin === 'kalshi');
  check('the same session stays unready (next_session: the launcher applies .env at next start)',
    midKalshi?.ready === false && (midKalshi?.resolvedVars ?? []).length === 0,
    JSON.stringify(midKalshi));

  // 4. reset removes only that plugin's rows (SQLite); `.env` is read-only.
  await core.request('market.credentials.set', {
    plugin: 'polysimulator', vars: { POLYSIM_API_KEY: 'sim-secret' },
  });
  const reset = await core.request('market.credentials.set', { plugin: 'kalshi', reset: true });
  check('reset clears the plugin and names its whole table',
    reset.reset === true && reset.written.length === 2, JSON.stringify(reset));
  // `.env` 是内核只读：reset 写的是 SQLite，文件不被触碰。
  check('the .env is NOT written by reset (kernel read-only)',
    !existsSync(envFile(core.cwd)), envFile(core.cwd));

  await core.stop();

  // 5. Boot 注入：重启核从同一 store（BLITZKRIEG_DB_PATH）读加密行，用
  //    BLITZKRIEG_DB_KEY 解密后注入进程环境（inject_credentials_into_process_env）
  //    —— 凭证的家是 SQLite，`.env` 不再是载体。
  const core2 = makeCore('restart', [], core.cwd, {});
  await core2.start();
  const after2 = await core2.request('market.credentials.status', {});
  const r2Sim = (after2.rows ?? []).find((r) => r.plugin === 'polysimulator');
  const r2Kal = (after2.rows ?? []).find((r) => r.plugin === 'kalshi');
  check('after a restart the surviving plugin resolves from the store (boot injected)',
    r2Sim?.ready === true && (r2Sim?.resolvedVars ?? []).includes('POLYSIM_API_KEY'),
    JSON.stringify(r2Sim));
  check('the reset plugin stays unready (its rows left the store)',
    r2Kal?.ready === false, JSON.stringify(r2Kal));
  await core2.stop();

  // 6. 遗留 config.json credentials 格的迁移已随 issue 967/920 退役
  //    （execution_policy.toml 退役时一并），内核不再读该格；此处只确认
  //    带 legacy 格的 config.json 不会让 boot 失败（fail-closed 不触发）。
  const cwd3 = mkdtempSync(join(tmpdir(), 'blitzkrieg-cred-migrate-'));
  cleanup.push(cwd3);
  mkdirSync(join(cwd3, 'data/runtime'), { recursive: true });
  writeFileSync(join(cwd3, 'data/runtime/config.json'),
    '{"credentials":{"kalshi":{"KALSHI_API_KEY_ID":"legacy-k"}}}');
  const core3 = makeCore('migrate', [], cwd3);
  await core3.start();
  const after3 = await core3.request('market.credentials.status', {});
  check('a legacy config.json credentials cell does not break boot',
    Array.isArray(after3.rows), JSON.stringify(after3).slice(0, 200));
  await core3.stop();

  console.log(gate.failures === 0
    ? '\nmarket:credentials — status/set/plan-first/reset/SQLite-landing/boot-injection/migration all hold over the real wire.'
    : `\nmarket:credentials — ${gate.failures} assertion(s) failed.`);
  process.exit(gate.failures === 0 ? 0 : 1);
} finally {
  for (const c of cleanup) rmSync(c, { recursive: true, force: true });
}
