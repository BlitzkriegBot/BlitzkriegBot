#!/usr/bin/env node
/**
 * dist-asset-check — the panel bundle integrity gate (#1199 eleventh audit).
 *
 * Two modes:
 *   (default) — post-build: parse dist/index.html's asset references, require
 *     every referenced file to exist under dist/assets/, then write
 *     dist/assets/manifest.sha256 covering EVERY file in dist/assets/ (lazy
 *     page chunks never appear in index.html, so the manifest walks the
 *     directory, not the references).
 *   --verify — post-deploy/upgrade: recompute every sha256 in the manifest and
 *     diff. A tampered or stale asset (old chunk beside a new manifest, or a
 *     new chunk the manifest never saw) reds. This is the arm that catches
 *     "the panel was rebuilt dirty / the bundle was assembled from mixed
 *     sources".
 *   --self-test — teeth on the same predicates buildMode/verifyMode use
 *     (reference parsing, existence, sha256 comparison), no tree touched.
 *
 * Zero dependencies (DEV.md §6.2 house style). Exit 0 pass / 1 violations.
 */

import { createHash } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'ui', 'webapp', 'webui', 'dist');
const INDEX = join(DIST, 'index.html');
const MANIFEST = join(DIST, 'assets', 'manifest.sha256');

function sha256(buf) {
  return createHash('sha256').update(buf).digest('hex');
}

/** Asset filenames referenced by index.html (/panel/assets/x, /assets/x or assets/x). */
function assetRefs(html) {
  const refs = new Set();
  const re = /(?:\/panel)?\/?assets\/([A-Za-z0-9._-]+)/g;
  for (const m of html.matchAll(re)) refs.add(m[1]);
  return refs;
}

/** Every file under dist/assets/ except the manifest itself. */
function assetFiles(dist) {
  const dir = join(dist, 'assets');
  if (!existsSync(dir)) return [];
  return readdirSync(dir).filter((f) => f !== 'manifest.sha256');
}

/** The post-build arm: reference check + manifest write. */
function buildMode() {
  if (!existsSync(INDEX)) {
    console.error(`dist-asset-check: ${INDEX} not found — build the panel first`);
    process.exit(1);
  }
  const refs = assetRefs(readFileSync(INDEX, 'utf8'));
  const missing = [...refs].filter((f) => !existsSync(join(DIST, 'assets', f)));
  if (missing.length) {
    console.error('dist-asset-check: index.html references missing assets:');
    for (const f of missing) console.error(`  + ${f}`);
    process.exit(1);
  }
  const lines = assetFiles(DIST)
    .map((f) => `${sha256(readFileSync(join(DIST, 'assets', f)))}  ${f}`)
    .sort();
  mkdirSync(join(DIST, 'assets'), { recursive: true });
  writeFileSync(MANIFEST, lines.join('\n') + '\n');
  console.log(`dist-asset-check: ${refs.size} index.html refs resolve; manifest covers ${lines.length} asset(s)`);
}

/** The post-deploy arm: recompute and diff against the manifest. */
function verifyMode() {
  if (!existsSync(MANIFEST)) {
    console.error(`dist-asset-check: ${MANIFEST} not found — run the build mode first`);
    process.exit(1);
  }
  const expected = new Map();
  for (const line of readFileSync(MANIFEST, 'utf8').split('\n')) {
    const m = line.match(/^([0-9a-f]{64})  (.+)$/);
    if (m) expected.set(m[2], m[1]);
  }
  let failures = 0;
  const report = (label) => {
    failures += 1;
    console.error(`  RED ${label}`);
  };
  for (const [file, want] of expected) {
    const path = join(DIST, 'assets', file);
    if (!existsSync(path)) {
      report(`${file} is in the manifest but missing on disk`);
      continue;
    }
    const got = sha256(readFileSync(path));
    if (got !== want) report(`${file} sha256 mismatch (manifest ${want.slice(0, 12)}…, disk ${got.slice(0, 12)}…)`);
  }
  for (const file of assetFiles(DIST)) {
    if (!expected.has(file)) report(`${file} is on disk but not in the manifest (stale or unrecorded asset)`);
  }
  if (failures) {
    console.error(`dist-asset-check: FAIL — ${failures} violation(s)`);
    process.exit(1);
  }
  console.log(`dist-asset-check: PASS — ${expected.size} asset(s) match the manifest`);
}

/** Teeth on the same predicates buildMode/verifyMode use — no tree touched. */
function selfTest() {
  const checks = [];
  const expect = (name, ok) => checks.push({ name, ok });

  // 1. reference parsing: both spellings, /panel prefix stripped.
  const refs = assetRefs(
    '<script src="/panel/assets/main-abc.js"></script><link href="assets/style.css">',
  );
  expect('assetRefs finds /panel/assets and assets spellings', refs.has('main-abc.js') && refs.has('style.css'));

  // 2. sha256 stability and content sensitivity.
  expect('sha256 is stable', sha256(Buffer.from('a')) === sha256(Buffer.from('a')));
  expect('sha256 differs on different content', sha256(Buffer.from('a')) !== sha256(Buffer.from('b')));

  // 3. dangling reference: the same predicate buildMode uses for its refusal.
  const dangling = [...assetRefs('<script src="/panel/assets/gone.js"></script>')]
    .filter((f) => !existsSync(join(DIST, 'assets', f)));
  expect('dangling reference detected', dangling.includes('gone.js'));

  // 4. tamper detection: the same comparison verifyMode uses.
  const want = sha256(Buffer.from('content'));
  const got = sha256(Buffer.from('tampered'));
  expect('tampered asset fails the sha256 comparison', want !== got);

  const failed = checks.filter((c) => !c.ok);
  for (const c of checks) console.log(`${c.ok ? 'ok  ' : 'RED '} self-test: ${c.name}`);
  if (failed.length) {
    console.error(`\ndist-asset-check self-test: ${failed.length} RED`);
    process.exit(1);
  }
  console.log('\ndist-asset-check self-test: all green');
}

if (process.argv.includes('--self-test')) {
  selfTest();
} else if (process.argv.includes('--verify')) {
  verifyMode();
} else {
  buildMode();
}