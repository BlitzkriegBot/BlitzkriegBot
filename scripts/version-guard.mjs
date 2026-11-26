#!/usr/bin/env node
/**
 * Version-consistency guard (VERSIONING.md §3 / §8.5).
 *
 * Four checks, each able to fail on its own:
 *   1. the root Cargo.toml has [workspace.package].version, and it is valid semver;
 *   2. every workspace member either inherits (`version.workspace = true`) or is
 *      on the allowlist (build tool / independent nested workspace);
 *   3. every version `cargo metadata` resolves to === the root version;
 *   4. the launcher binary's own self-description matches the manifest (when a
 *      binary exists at all — this is the N2 reverse-acceptance: a version bump
 *      without a rebuild must turn this red).
 *
 * Usage:
 *   node scripts/version-guard.mjs                 # full check
 *   node scripts/version-guard.mjs --manifest-only # manifest only (the CI tag guard)
 *
 * Exit codes: 0 all green · 1 inconsistency · 2 usage/environment error
 *
 * The launcher binary path follows the repo's BK_CORE_BIN convention
 * (scripts/lib/core-provenance.mjs): BK_LAUNCHER_BIN overrides the default
 * `target/release/blitzkrieg` so a gate can pin the exact binary it inspects
 * without building into the deployed path.
 */
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const manifestOnly = args.includes('--manifest-only');
if (args.some((a) => a !== '--manifest-only')) {
  console.error('usage: node scripts/version-guard.mjs [--manifest-only]');
  process.exit(2);
}

let failures = 0;
const check = (label, fn) => {
  try {
    fn();
    console.log(`  ok   ${label}`);
  } catch (e) {
    failures++;
    console.log(`  FAIL ${label}\n       ${e.message}`);
  }
};

const read = (...p) => readFileSync(join(ROOT, ...p), 'utf8');

/** Crates allowed to keep a literal version, with the reason spelled out. */
const NO_INHERIT = new Set([
  'blitzkrieg-build-support', // build-time tool, not part of the product (§3.2)
  'blitzkrieg-webapp', // independent nested workspace (tauri shell) — cannot inherit
]);

// ── 1. The root manifest ─────────────────────────────────────────────────────
const rootManifest = read('Cargo.toml');
const wsPkg = rootManifest.match(/^\[workspace\.package\]([\s\S]*?)^\[/m)?.[1] ?? '';
const version = wsPkg.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1] ?? null;

check('root Cargo.toml has [workspace.package].version', () => {
  if (!wsPkg) throw new Error('no [workspace.package] table — the single source does not exist yet');
  if (!version) throw new Error('no version key inside [workspace.package]');
});

check('the version is valid semver (optionally -rc.N)', () => {
  if (!/^\d+\.\d+\.\d+(-rc\.\d+)?$/.test(version ?? '')) {
    throw new Error(`not <major>.<minor>.<patch>[-rc.N]: ${JSON.stringify(version)}`);
  }
});

// ── 1b. The tauri shell's own version pair (V8-6) ───────────────────────────
// src-tauri is a deliberately independent workspace (it cannot inherit the root
// version), so its two version LITERALS must at least agree with each other.
// Both files get edited by hand sooner or later — two places, one must win.
check('src-tauri/Cargo.toml and tauri.conf.json versions are equal', () => {
  const cargo = read('ui', 'webapp', 'src-tauri', 'Cargo.toml');
  const conf = read('ui', 'webapp', 'src-tauri', 'tauri.conf.json');
  const cargoV = cargo.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1] ?? null;
  const confV = JSON.parse(conf).version ?? null;
  if (!cargoV || !confV) {
    throw new Error(`tauri version missing (Cargo.toml ${JSON.stringify(cargoV)}, conf ${JSON.stringify(confV)})`);
  }
  if (cargoV !== confV) {
    throw new Error(`tauri shell versions diverge: Cargo.toml ${cargoV} vs tauri.conf.json ${confV}`);
  }
});

// The workspace members named in the root manifest. Trailing TOML comments after
// the comma are tolerated; a member entry is a quoted path ending in a comma,
// and nothing else in the root manifest has that shape on its own line (the
// `exclude = [...]` line starts with a key).
const members = [
  ...rootManifest.matchAll(/^\s*"((?:core|ui|extensions|user_layer)\/[\w\-/]+)",\s*(?:#.*)?$/gm),
].map((m) => m[1]);

// ── 1c. The nested workspaces' locks must track the root version ────────────
// The root manifest's `exclude` list carves out the stand-alone workspaces that
// each carry their own Cargo.lock (mirroring a real third-party author's
// checkout). A version bump edits the root manifest and the root lock; nothing
// reminds you that these locks pin the *same* path crates to the old version.
// The failure is not cosmetic: every `cargo build --release --locked` in them
// then refuses to run — `--locked was passed to prevent this` — and the release
// upgrade dies on its build step with an otherwise green tree. Reads manifests
// and locks only, so it also runs under --manifest-only.
check('nested workspace locks track the root version', () => {
  const excluded =
    rootManifest.match(/^\s*exclude\s*=\s*\[([^\]]*)\]/m)?.[1].match(/"([^"]+)"/g)?.map((s) => s.slice(1, -1)) ?? [];
  // A silent scan failure must not turn this into a check that can never fail:
  // with no excluded dirs or no known member names there is nothing to compare.
  if (excluded.length === 0) {
    throw new Error('no `exclude` list found in the root manifest — the scan is broken, not the tree');
  }
  // Only the crates that inherit the root version are ours to pin: a nested
  // workspace's own crates (parity_strategy, momentum_alpha, …) carry literal
  // versions of their own and must be left alone.
  const ours = new Set();
  for (const m of members) {
    try {
      const name = read(m, 'Cargo.toml').match(/^name\s*=\s*"([^"]+)"/m)?.[1];
      if (name) ours.add(name);
    } catch {
      // A member whose manifest cannot be read is check #2's problem, not ours.
    }
  }
  if (ours.size === 0) {
    throw new Error('no workspace member names resolved — the member scan is broken, not the tree');
  }
  const stale = [];
  let inspected = 0;
  for (const dir of excluded) {
    let lock;
    try {
      lock = read(dir, 'Cargo.lock');
    } catch {
      continue; // not every excluded workspace keeps a lock in the repo (src-tauri's is ignored)
    }
    for (const block of lock.split('[[package]]').slice(1)) {
      if (/^\s*source\s*=/m.test(block)) continue; // registry/git dep, not a path crate
      const name = block.match(/^\s*name\s*=\s*"([^"]+)"/m)?.[1];
      const ver = block.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1];
      if (!name || !ver || !ours.has(name)) continue;
      inspected++;
      if (ver !== version) stale.push(`${dir}: ${name} = ${ver}`);
    }
  }
  // Same reasoning as above: comparing zero packages is not a pass.
  if (inspected === 0) {
    throw new Error('no root-versioned path crates found in any nested lock — the lock scan is broken');
  }
  if (stale.length) {
    throw new Error(
      `${stale.join(', ')}\n       expected ${version} — refresh the lock from its own directory ` +
        `(\`cargo build --release\` or \`cargo update --workspace\`) and commit it`,
    );
  }
});

if (manifestOnly) {
  console.log(failures ? `\nversion-guard: ${failures} failure(s)` : '\nversion-guard: ok');
  process.exit(failures ? 1 : 0);
}

// ── 2. Member manifests: inherit, or be on the allowlist ─────────────────────
check('the member scan finds the workspace members', () => {
  if (members.length === 0) {
    throw new Error('no workspace members matched — the member scan regex is broken');
  }
  console.log(`  ...  ${members.length} members: ${members.join(', ')}`);
});

check('every workspace member inherits the workspace version', () => {
  const offenders = [];
  for (const m of members) {
    let text;
    try {
      text = read(m, 'Cargo.toml');
    } catch {
      continue; // a member whose manifest cannot be read is check #3's problem
    }
    const name = text.match(/^name\s*=\s*"([^"]+)"/m)?.[1] ?? m;
    if (NO_INHERIT.has(name)) continue;
    if (!/^\s*version\.workspace\s*=\s*true/m.test(text)) offenders.push(`${m} (${name})`);
  }
  if (offenders.length) {
    throw new Error(
      `members with a literal version: ${offenders.join(', ')}\n       use \`version.workspace = true\``,
    );
  }
});

check('no member still carries a literal version', () => {
  const offenders = [];
  for (const m of members) {
    let text;
    try {
      text = read(m, 'Cargo.toml');
    } catch {
      continue;
    }
    const name = text.match(/^name\s*=\s*"([^"]+)"/m)?.[1] ?? m;
    if (NO_INHERIT.has(name)) continue;
    const lit = text.match(/^\s*version\s*=\s*"([^"]+)"/m);
    if (lit) offenders.push(`${m} = ${lit[1]}`);
  }
  if (offenders.length) throw new Error(offenders.join(', '));
});

// ── 3. What cargo actually resolves ──────────────────────────────────────────
check('every member version cargo resolves to === the root version', () => {
  const meta = JSON.parse(
    execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {
      cwd: ROOT,
      encoding: 'utf8',
    }),
  );
  const bad = meta.packages
    .filter((p) => !NO_INHERIT.has(p.name))
    .filter((p) => p.version !== version)
    .map((p) => `${p.name}=${p.version}`);
  if (bad.length) throw new Error(`expected all = ${version}: ${bad.join(', ')}`);
});

// ── 4. The runtime self-description must match the repo ─────────────────────
check('the launcher binary self-describes the manifest version', () => {
  const bin = process.env.BK_LAUNCHER_BIN || join(ROOT, 'target', 'release', 'blitzkrieg');
  let out;
  try {
    out = execFileSync(bin, ['version', '--json'], { encoding: 'utf8', timeout: 20_000 }).trim();
  } catch {
    console.log(
      `  note no answerable binary at ${bin} — run \`cargo build --release --workspace --locked\` to enable this check`,
    );
    return;
  }
  let got;
  try {
    got = JSON.parse(out).version;
  } catch {
    throw new Error(`binary printed unparseable JSON: ${JSON.stringify(out.slice(0, 200))}`);
  }
  if (got !== version) {
    throw new Error(`binary says ${got}, manifest says ${version} — rebuild (N2 in VERSIONING.md §10.2)`);
  }
});

console.log(failures ? `\nversion-guard: ${failures} failure(s)` : '\nversion-guard: ok');
process.exit(failures ? 1 : 0);
