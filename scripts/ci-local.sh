#!/usr/bin/env bash
# ci-local.sh — 本地复刻 GitHub Actions CI 的全部步骤
# 用途：推送前本地跑一遍，确保与 GitHub CI 完全等价
# 用法：cd <worktree> && bash scripts/ci-local.sh
#
# 与 GitHub CI 的对应关系（ci.yml）：
#   planner → 本地直接跑全部步骤（不走 lane 判断，本地全量跑）
#   rust-check → 步骤 1-12
#   default-feature-tests → 步骤 13
#   panel-check → 步骤 14-15（UI 前端 + gate 脚本）
#   core-gates → 步骤 16-20（Node gate 脚本）
#   secret-scan → 步骤 21
#   file-size → 步骤 22
#   store-layer → 步骤 23
#
# 注意：本地是 macOS arm64，CI 是 Linux x86_64。某些平台差异（如 E14 内存基线的 RSS 天花板）CI 脚本会自己处理。

set -euo pipefail
# 与 CI 完全等价：不设 CARGO_TARGET_DIR（CI 是全新 checkout，用默认 target/）。
# 若本地 shell 已导出 CARGO_TARGET_DIR（共享缓存），必须 unset，否则 gate
# 脚本（intent-audit-check 等）自己 cargo build 的探针 dylib 会落到共享目录，
# 找不到 user_layer/strategies/<name>/target/release/ 下的产物。
unset CARGO_TARGET_DIR

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

step() { echo -e "\n${YELLOW}▶ $1${NC}"; }
pass() { echo -e "${GREEN}✓ $1${NC}"; }
fail() { echo -e "${RED}✗ $1${NC}"; exit 1; }

# ── 1. planner 守卫（dep-direction + contract-tree + store-layer） ──────────
step "planner: dep-direction-check --self-test + real + teeth"
node scripts/dep-direction-check.mjs --self-test || fail "dep-direction self-test"
node scripts/dep-direction-check.mjs || fail "dep-direction real"
sh scripts/dep-direction-check-teeth.sh || fail "dep-direction teeth"
pass "dep-direction"

step "planner: contract-tree-check --self-test + real"
node scripts/contract-tree-check.mjs --self-test || fail "contract-tree self-test"
node scripts/contract-tree-check.mjs || fail "contract-tree real"
pass "contract-tree"

step "planner: store-layer-check --self-test + real"
node scripts/store-layer-check.mjs --self-test || fail "store-layer self-test"
node scripts/store-layer-check.mjs || fail "store-layer real"
pass "store-layer"

# ── 2. rust-check 步骤 ──────────────────────────────────────────────────────
step "rust-check: build reference cdylibs (standalone workspaces)"
(cd user_layer/parity_strategy && cargo build --release --locked) || fail "parity_strategy build"
(cd user_layer/optional_capability_fixture && cargo build --release --locked) || fail "optional_capability_fixture build"
pass "reference cdylibs"

step "rust-check: build (release, workspace, locked)"
cargo build --release --workspace --locked || fail "release build"
pass "release build"

step "rust-check: binary & repo size budget"
node scripts/binary-size-check.mjs --verbose || fail "binary-size"
pass "binary-size"

step "rust-check: test (workspace, locked, test-support, BK_REQUIRE_DYLIB)"
BK_REQUIRE_DYLIB=1 cargo test --workspace --locked --features blitzkrieg-core/test-support || fail "test with test-support"
pass "test with test-support"

step "rust-check: rustfmt"
cargo fmt --all -- --check || fail "rustfmt"
pass "rustfmt"

step "rust-check: clippy (workspace, all-targets, -D warnings, test-support)"
cargo clippy --workspace --all-targets --locked --features blitzkrieg-core/test-support -- -D warnings || fail "clippy"
pass "clippy"

step "rust-check: version guard"
node scripts/version-guard.mjs --manifest-only || fail "version-guard"
pass "version-guard"

# ── 3. default-feature-tests ────────────────────────────────────────────────
step "default-feature-tests: test (default features, workspace, locked)"
cargo test --workspace --locked || fail "default-feature test"
pass "default-feature-tests"

# ── 4. panel-check（UI 前端 + gate 脚本）────────────────────────────────────
step "panel-check: build gateway and core (release)"
cargo build --release --locked -p blitzkrieg-ui-kit -p blitzkrieg-launcher -p blitzkrieg-ui-panel || fail "panel build"
pass "panel build"

step "panel-check: build panel bundle"
(cd ui/webapp/webui && npm ci && npm run build) || fail "panel bundle"
pass "panel bundle"

step "panel-check: panel checks (theme/balance/lifecycle/rejections/feed/session/history/round/expand)"
(cd ui/webapp/webui && npm run check && npm run check:all) || fail "panel checks"
pass "panel checks"

step "panel-check: ui:webapp gate"
node scripts/webapp-check.mjs || fail "webapp gate"
pass "webapp gate"

# ── 5. core-gates（Node gate 脚本，E24..E30）─────────────────────────────
step "core-gates: v0.3 gates teeth"
node scripts/intent-audit-check.mjs --teeth || fail "intent-audit teeth"
node scripts/strategy-no-stop-loss-check.mjs --teeth || fail "no-stop-loss teeth"
node scripts/strategy-declaration-check.mjs --teeth || fail "strategy-declaration teeth"
node scripts/plugin-modes-check.mjs --teeth || fail "plugin-modes teeth"
node scripts/kline-aggregation-check.mjs --teeth || fail "kline teeth"
node scripts/risk-systemic-check.mjs --teeth || fail "risk-systemic teeth"
node scripts/account-credential-check.mjs --teeth || fail "account-credential teeth"
node scripts/account-cross-isolation-check.mjs --teeth || fail "account-cross-isolation teeth"
node scripts/lua-sandbox-check.mjs --teeth || fail "lua-sandbox teeth"
node scripts/lua-decimal-check.mjs --teeth || fail "lua-decimal teeth"
node scripts/thesis-death-check.mjs --teeth || fail "thesis-death teeth"
pass "v0.3 gates teeth"

step "core-gates: v0.3 gates real verdicts"
node scripts/intent-audit-check.mjs || fail "intent-audit"
node scripts/strategy-no-stop-loss-check.mjs || fail "no-stop-loss"
node scripts/strategy-declaration-check.mjs || fail "strategy-declaration"
node scripts/plugin-modes-check.mjs || fail "plugin-modes"
node scripts/kline-aggregation-check.mjs || fail "kline"
node scripts/risk-systemic-check.mjs || fail "risk-systemic"
node scripts/account-credential-check.mjs || fail "account-credential"
node scripts/account-cross-isolation-check.mjs || fail "account-cross-isolation"
node scripts/lua-sandbox-check.mjs || fail "lua-sandbox"
node scripts/lua-decimal-check.mjs || fail "lua-decimal"
node scripts/thesis-death-check.mjs || fail "thesis-death"
pass "v0.3 gates real verdicts"

step "core-gates: file-size ratchet"
node scripts/file-size-check.mjs --self-test || fail "file-size self-test"
node scripts/file-size-check.mjs || fail "file-size"
pass "file-size"

step "core-gates: contract-tree"
node scripts/contract-tree-check.mjs || fail "contract-tree"
pass "contract-tree"

# ── 6. secret-scan ─────────────────────────────────────────────────────────
step "secret-scan"
bash scripts/secret-scan.sh || fail "secret-scan"
pass "secret-scan"

echo -e "\n${GREEN}══════════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}  ALL LOCAL CI CHECKS PASSED — 可以推送了${NC}"
echo -e "${GREEN}══════════════════════════════════════════════════════════════${NC}"