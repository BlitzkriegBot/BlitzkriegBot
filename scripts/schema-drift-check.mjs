#!/usr/bin/env node
/**
 * schema-drift-check — cross-language Rust ↔ TypeScript constant parity gate (#281).
 *
 * Mirrors the net-check.check.mjs pattern for the three hand-mirrored constant
 * tables that lack a runtime schema endpoint (or keep local fallbacks):
 *   1. execution_policy ConditionField::field_name() ↔ webui src/lib/policy-fields.ts CONDITION_FIELDS
 *   2. cockpit COCKPIT_KEYS ↔ webui src/pages/settings/SettingsRiskLimits.vue field bindings
 *   3. blueprint CONDITION_OPS & NodeKind::parse() ↔ webui src/lib/blueprint.ts CONDITION_OPS & NODE_SPECS
 *
 * Usage:
 *   node scripts/schema-drift-check.mjs
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const read = (...p) => readFileSync(resolve(ROOT, ...p), 'utf8')

let failures = 0
const check = (label, fn) => {
  try {
    fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.error(`  FAIL ${label}\n       ${e.message}`)
  }
}

console.log('schema-drift-check: Rust ↔ TS constant parity (#281)')

// ── 1. execution_policy ConditionField ↔ policy-fields.ts CONDITION_FIELDS ──
check('policy.CONDITION_FIELDS matches Rust ConditionField::field_name()', () => {
  const rustSrc = read('core/blitzkrieg_core/src/execution_policy/mod.rs')
  const fnMatch = rustSrc.match(/fn field_name\(&?self\) -> &'static str \{[\s\S]*?\n    \}/)
  assert.ok(fnMatch, 'ConditionField::field_name() not found in execution_policy/mod.rs')
  const rustKeys = [...fnMatch[0].matchAll(/=>\s*"([a-z0-9_]+)"/g)].map((m) => m[1]).sort()
  assert.ok(rustKeys.length > 0, 'extracted 0 keys from ConditionField::field_name()')

  const tsSrc = read('ui/webapp/webui/src/lib/policy-fields.ts')
  const arrMatch = tsSrc.match(/export const CONDITION_FIELDS[\s\S]*?\] as const/)
  assert.ok(arrMatch, 'CONDITION_FIELDS not found in policy-fields.ts')
  const tsKeys = [...arrMatch[0].matchAll(/value:\s*'([a-z0-9_]+)'/g)].map((m) => m[1]).sort()
  assert.deepEqual(tsKeys, rustKeys, 'policy CONDITION_FIELDS drifted between Rust and TS')
})

// ── 2. cockpit COCKPIT_KEYS ↔ SettingsRiskLimits.vue ──────────────────────
check('cockpit.COCKPIT_KEYS are all bound in SettingsRiskLimits.vue', () => {
  const rustSrc = read('core/blitzkrieg_core/src/cockpit.rs')
  const tblMatch = rustSrc.match(/pub const COCKPIT_KEYS: &\[CockpitKey\] = &\[[\s\S]*?\n\];/)
  assert.ok(tblMatch, 'COCKPIT_KEYS not found in cockpit.rs')
  const rustFields = [...tblMatch[0].matchAll(/field:\s*"([A-Za-z0-9_]+)"/g)].map((m) => m[1]).sort()
  assert.ok(rustFields.length > 0, 'extracted 0 fields from COCKPIT_KEYS')

  const vueSrc = read('ui/webapp/webui/src/pages/settings/SettingsRiskLimits.vue')
  const missing = rustFields.filter((f) => !new RegExp(`\\b${f}\\b`).test(vueSrc))
  assert.deepEqual(
    missing,
    [],
    `SettingsRiskLimits.vue is missing COCKPIT_KEYS fields: ${missing.join(', ')}`,
  )
})

// ── 3. blueprint CONDITION_OPS & NodeKind::parse() ↔ blueprint.ts ─────────
check('blueprint.CONDITION_OPS matches Rust blueprint.rs CONDITION_OPS', () => {
  const rustSrc = read('core/blitzkrieg_core/src/blueprint.rs')
  const rustMatch = rustSrc.match(/pub const CONDITION_OPS:[^=]*=\s*&?\[([^\]]+)\]/)
  assert.ok(rustMatch, 'CONDITION_OPS not found in blueprint.rs')
  const rustOps = [...rustMatch[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]).sort()
  assert.ok(rustOps.length > 0, 'extracted 0 ops from blueprint.rs CONDITION_OPS')

  const tsSrc = read('ui/webapp/webui/src/lib/blueprint.ts')
  const tsMatch = tsSrc.match(/export const CONDITION_OPS\s*=\s*\[([^\]]+)\]/)
  assert.ok(tsMatch, 'CONDITION_OPS not found in blueprint.ts')
  const tsOps = [...tsMatch[1].matchAll(/'([^']+)'/g)].map((m) => m[1]).sort()
  assert.deepEqual(tsOps, rustOps, 'blueprint CONDITION_OPS drifted between Rust and TS')
})

check('blueprint.NODE_SPECS matches Rust NodeKind::parse() node types', () => {
  const rustSrc = read('core/blitzkrieg_core/src/blueprint.rs')
  const parseMatch = rustSrc.match(/fn parse\(node_type: &str, id: &str\) -> Result<Self, String> \{([\s\S]*?)other =>/)
  assert.ok(parseMatch, 'NodeKind::parse() not found in blueprint.rs')
  const rustKinds = [...parseMatch[1].matchAll(/"([a-z0-9_]+)"/g)].map((m) => m[1]).sort()
  assert.ok(rustKinds.length > 0, 'extracted 0 node types from NodeKind::parse()')

  const tsSrc = read('ui/webapp/webui/src/lib/blueprint.ts')
  const specMatch = tsSrc.match(/export const NODE_SPECS: NodeSpec\[\] = \[([\s\S]*?)\n\]/)
  assert.ok(specMatch, 'NODE_SPECS not found in blueprint.ts')
  const tsKinds = [...specMatch[1].matchAll(/type:\s*'([a-z0-9_]+)'/g)].map((m) => m[1]).sort()
  assert.deepEqual(tsKinds, rustKinds, 'blueprint NODE_SPECS drifted from Rust NodeKind::parse()')
})

if (failures > 0) {
  console.error(`\nschema-drift-check: ${failures} FAIL`)
  process.exit(1)
}
console.log('\nRESULT: PASS')
