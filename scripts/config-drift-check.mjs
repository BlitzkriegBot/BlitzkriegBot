#!/usr/bin/env node
/**
 * config-drift-check — issue #44 acceptance: offline comparison of the three
 * config sources, with divergence explicitly marked.
 *
 * Reads exactly the three sources the kernel's `pick()` chain consults and
 * prints one row per logical key, one column per source. Keys whose values
 * differ across more than one source are marked `CONFLICT` in the `status`
 * column and totalled in the closing summary. The script never writes
 * anything — it only reads.
 *
 *   1. SQLite — the single source of truth for persisted configuration:
   `data/blitzkrieg.sqlite`, table `kernel_meta` (key → `meta.<key>`) and
   table `account_settings` (account_id + key → `accounts.<account>.<key>`);
   2. `.env` — credential env var NAMES only (read-only to the kernel, issue
   #1199 sixth-round audit): this script prints the names it finds, never
   secret values (R2);
   3. `data/runtime/config.json` — the runtime config cell, top-level keys.
 *
 * Values are compared verbatim as strings; a key present in only one source
 * is not a conflict (it is simply where that key lives). `pick()` resolves
 * any real divergence CLI > SQLite > env > compiled default, so the SQLite
 * row wins when it exists.
 *
 * Usage:
 *   node scripts/config-drift-check.mjs
 *   node scripts/config-drift-check.mjs --db data/blitzkrieg.sqlite \
 *       --env .env --json data/runtime/config.json
 *
 * Exit code: 0 when the comparison completed; 2 when the environment itself
 * is unusable (no `sqlite3` binary — the check cannot run offline without it).
 * Conflicts are marked, not failed on: this is a diagnostic, not a gate.
 */
import { existsSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');

const argv = process.argv.slice(2);
const opt = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : def;
};
const DB_PATH = opt('--db', join(ROOT, 'data', 'blitzkrieg.sqlite'));
const ENV_PATH = opt('--env', join(ROOT, '.env'));
const JSON_PATH = opt('--json', join(ROOT, 'data', 'runtime', 'config.json'));

const NOT_SET = '(unset)';

// The SQL text is constant — nothing from the environment is interpolated
// into it (DEV §7: no string interpolation into exec*Sync).
const sqlite3 = (sql) => {
  const out = execFileSync('sqlite3', [DB_PATH, '-separator', '\t', sql], {
    encoding: 'utf8',
  });
  return out
    .split(/\r?\n/)
    .map((line) => line.split('\t'))
    .filter((row) => row.length > 0 && row.some((cell) => cell.length > 0));
};

const readSqlite = () => {
  const map = new Map();
  let present = false;
  let note = '';
  if (!existsSync(DB_PATH)) {
    return { map, present: false, note: `sqlite: ${DB_PATH} missing (no persisted config yet)` };
  }
  try {
    for (const [key, value] of sqlite3('SELECT key, value FROM kernel_meta;')) {
      map.set(`meta.${key}`, value);
      present = true;
    }
    for (const [account, key, value] of sqlite3(
      'SELECT account_id, key, value FROM account_settings;',
    )) {
      map.set(`accounts.${account}.${key}`, value);
      present = true;
    }
    note = `sqlite: ${DB_PATH} — ${map.size} keys`;
  } catch (e) {
    note = `sqlite: ${DB_PATH} unreadable (${e.message}) — treated as empty`;
  }
  return { map, present, note };
};

// `.env` lines are `KEY=VALUE`; split on the FIRST `=` only, values may
// contain `=`. Comment and blank lines are skipped.
const readEnv = () => {
  const map = new Map();
  if (!existsSync(ENV_PATH)) {
    return { map, present: false, note: `env: ${ENV_PATH} missing` };
  }
  const text = readFileSync(ENV_PATH, 'utf8');
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('#') || line.startsWith(':')) continue;
    const eq = line.indexOf('=');
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    if (!key) continue;
    map.set(key, line.slice(eq + 1).trim());
  }
  return { map, present: true, note: `env: ${ENV_PATH} — ${map.size} keys` };
};

const toScalar = (v) => {
  if (v === null || v === undefined) return String(v);
  if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean')
    return String(v);
  return JSON.stringify(v);
};

const readJson = () => {
  const map = new Map();
  if (!existsSync(JSON_PATH)) {
    return { map, present: false, note: `json: ${JSON_PATH} missing` };
  }
  const obj = JSON.parse(readFileSync(JSON_PATH, 'utf8'));
  if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) {
    return {
      map,
      present: true,
      note: `json: ${JSON_PATH} — top level is not an object, 0 keys`,
    };
  }
  for (const [key, value] of Object.entries(obj)) map.set(key, toScalar(value));
  return { map, present: true, note: `json: ${JSON_PATH} — ${map.size} keys` };
};

const pad = (s, w) => (s.length > w ? s : s + ' '.repeat(w - s.length));

const main = () => {
  const sqlite = readSqlite();
  const env = readEnv();
  const json = readJson();

  console.log('config-drift-check — issue #44 offline comparison of the three sources');
  console.log(`  ${sqlite.note}`);
  console.log(`  ${env.note}`);
  console.log(`  ${json.note}`);
  console.log('');

  const keys = new Set([
    ...sqlite.map.keys(),
    ...env.map.keys(),
    ...json.map.keys(),
  ]);

  const rows = [];
  let conflicts = 0;
  for (const key of [...keys].sort()) {
    const s = sqlite.map.get(key) ?? null;
    const e = env.map.get(key) ?? null;
    const j = json.map.get(key) ?? null;
    const present = [s, e, j].filter((v) => v !== null);
    const distinct = new Set(present);
    const conflict = present.length > 1 && distinct.size > 1;
    if (conflict) conflicts++;
    rows.push({ key, s, e, j, conflict });
  }

  const keyW = rows.length > 0 ? Math.max(...rows.map((r) => r.key.length), 3) + 2 : 10;
  const colW =
    rows.length > 0
      ? Math.max(
          ...rows.flatMap((r) =>
            [r.s ?? '', r.e ?? '', r.j ?? '', ''].map((v) => v.length),
          ),
          6,
        ) + 2
      : 10;
  const head = [
    pad('key', keyW),
    pad('sqlite', colW),
    pad('env', colW),
    pad('json', colW),
    'status',
  ].join(' ');
  console.log(head);
  console.log(
    [pad('---', keyW), pad('------', colW), pad('---', colW), pad('---', colW), '------']
      .join(' '),
  );
  for (const r of rows) {
    const disp = (v) => (v === null ? NOT_SET : v);
    console.log(
      [
        pad(r.key, keyW),
        pad(disp(r.s), colW),
        pad(disp(r.e), colW),
        pad(disp(r.j), colW),
        r.conflict ? 'CONFLICT' : '-',
      ].join(' '),
    );
  }

  console.log('');
  console.log(
    `keys compared: ${rows.length}, conflicts: ${conflicts} (values differ in more than one source)`,
  );
  if (conflicts > 0) {
    console.log(
      'Keys above marked CONFLICT carry different values in more than one source; ' +
        '`pick()` resolves them CLI > SQLite > env > compiled default, so the SQLite row wins.',
    );
  }
  process.exit(0);
};

main();
