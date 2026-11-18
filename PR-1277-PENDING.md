# PR #1277 待手动创建（本说明留在 worktree 内）

## 现状
分支 `fix/1277-pr` 已在 commit `482db07a` 上（基于 `ceer/main` = `fb8a1ebb`），内容为 #1277 胜率 single-口径 + 净 PnL 账本单锚，仅动 `core/blitzkrieg_core/`，未碰 `ui/`，未碰凭证。

## 本地门禁结果（已跑，真实输出见会话记录）
- `cargo build --release --workspace --locked` → 绿，`Finished ... in 4m 09s`
- `cargo test --workspace --locked` → 绿，全场景 0 failed；新增 `win_flat_loss_partitions_hundred_trades ... ok`
- 两个嵌套 workspace `cargo build --release --locked` → 绿

## PR 正文
已写到 `/tmp/pr-1277-body.md`（含标题、file:line 变更清单、门禁结果、win/flat/loss 三分区 + 100 笔回归测试说明、安全与约束确认）。

## 为什么还没开
`gh pr create ... --body-file <(mktemp)` 失败：`gh` 未认证，提示 `gh auth login`（等价 401），退出码 4。未阻塞。

## 下一步（user 执行）
1. `gh auth login`（或按 `dev-docs/GITHUB.md` §1.3 §7 以 token 走 REST），或直接在 GitHub Web 手动开 PR。
2. 开 PR：base = `ceer/main`，head = `fix/1277-pr`，标题 `fix(core): 胜率 single-口径 + 净 PnL 账本单锚 (#1277)`，正文用 `/tmp/pr-1277-body.md`。
3. PR 开出后由审核者（GPT6）处置 CI/合并；后端不 rerun CI、不 squash-merge（RULES §四）。

提交作者：`ceer_quant <ceer_quant@users.noreply.github.com>`，无 AI 署名（R11）。
