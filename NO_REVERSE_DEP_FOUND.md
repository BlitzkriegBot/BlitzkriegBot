# #1281 P1 — 内核反向依赖 user_layer/tools：核实结论

## 结论

**未发现反向依赖，亦无 Cargo 循环依赖。** 未做任何代码改动（ponytail/YAGNI：不消除不存在的问题；R5 禁顺手优化）。

## 核实范围

- `user_layer/` 及其子 crate：configs、examples、lua_runtime、optional_capability_fixture、parity_logic、parity_strategy、strategies、strategies_lua、strategy_api、strategy_logic
- `core/blitzkrieg_core` 对 `user_layer/*` 的依赖声明
- `core/blitzkrieg_core` 源码中对 user_layer crate 的 `use`（含 `#[cfg(feature = ...)]` 门控）
- `root tools/onchain_pull`（内核通过 `blitzkrieg-onchain-pull` 引用的唯一 root-tools crate，排查是否形成环）

## 关键事实

1. **不存在 `user_layer/tools` crate。**
   - `user_layer/` 下无 `tools/` 子目录；root `tools/` 仅含 `onchain_pull`（`blitzkrieg-onchain-pull`，非 `user_layer` 下的 crate）。

2. **`core/blitzkrieg_core → user_layer/*` 是单向正向依赖，并非反向依赖。**
   - `core/blitzkrieg_core/Cargo.toml:66` `blitzkrieg-strategy-api = { path = "../../user_layer/strategy_api" }`（主依赖，无 `optional`，无 feature 门控）
   - `core/blitzkrieg_core/Cargo.toml:70` `blitzkrieg-lua-runtime = { path = "../../user_layer/lua_runtime" }`（同上）
   - `core/blitzkrieg_core/Cargo.toml:74` `strategy-logic = { path = "../../user_layer/strategy_logic" }`（同上）
   - `core/blitzkrieg_core/Cargo.toml:121` `parity-logic`（仅 dev-dependency，不进 shipped library）
   - 源码无条件使用（`registry.rs:16`、`signal.rs:15-22`、`model.rs:24`、`service.rs:132`/`1278`、`backtest.rs:656` 等），与 Cargo.toml 主依赖一致，无 `cfg(feature)` 反向门控。

3. **无 `user_layer/* → core/blitzkrieg_core` 边。**
   - `grep -rn "blitzkrieg-core" user_layer/ --include=Cargo.toml` → 无输出。
   - `user_layer/strategy_api/Cargo.toml:16` 仅依赖 `blitzkrieg-market-api = core/market_api`（该叶子 crate 自称 deliberately has no internal dependencies，DEV_V0_3.md 行 999）。
   - `user_layer/lua_runtime/Cargo.toml:19` 仅依赖 `blitzkrieg-strategy-api`（不依赖 core）。
   - `user_layer/strategy_logic/Cargo.toml:13` 仅依赖 `blitzkrieg-market-api`（不依赖 core）。
   - `user_layer/parity_strategy/Cargo.toml:17-18` 依赖 `strategy_api` + `parity-logic`（不依赖 core）。

4. **依赖方向纪律符合 DEV_V0_3.md §9.6（行 989-1001）：** “不许出现反向边”；`strategy_api` 不得依赖 core；`lua_runtime ──► strategy_api`（只做类型对齐，不依赖 core）。实际图：`core → strategy_api/lua_runtime/strategy_logic`，`strategy_api/lua_runtime/strategy_logic → market_api`（叶子），无环。

## 处置

按任务要求：无反向依赖存在，故不做代码改动；本说明留存于 worktree 根 `NO_REVERSE_DEP_FOUND.md`，随后跑完全部门禁（`cargo build --release --workspace --locked`、`cargo test --workspace --locked`、两子 crate build、`cargo clippy --workspace --all-targets -- -D warnings`、`bash scripts/secret-scan.sh`）并贴真实输出。
