# IPC 出站背压 + 快照分页契约（#1282）

## 问题

内核 → UI 出站用 `mpsc::unbounded_channel::<String>`（`core/blitzkrieg_core/src/ipc/server/session.rs:1034`）：无上限，慢/离线客户端会让出站缓冲无限增长；写端无超时，一条会话阻塞/断连会拖死整条出站链。快照首屏一次性全量发送，payload 随持仓/订阅线性膨胀；历史 tab 无分页，全量 verb 回放。

## 契约

1. **出站通道有界**：把 unbounded 换成 `bounded(N)`（session.rs 附近）。**满队列策略：丢通知不丢响应**——notification 类消息可 best-effort 丢弃，response 类消息必须送达或按断连错误降级，决不阻塞写端，更不拖累其他会话。
2. **写端超时**：每条 write 带超时；超时判定客户端不可达，仅断开该会话，不阻塞内核及其他会话。
3. **读路径预限长**：`BufReader::lines()` 读取前按 `take(MAX_LINE_BYTES)`（`MAX_LINE_BYTES = 1024 * 1024`，`session.rs:1014`，与 `scanner.rs:1014` 对齐）提前截断；超长无换行输入：截断后断开连接，不缓冲无限长度的单行。
4. **快照首屏分页**：首屏默认 200 条，payload 须下降一个数量级；历史 tab 改走分页 verb（offset/limit 或 cursor），不再全量回放。
5. **验收**：离线压测 RSS 有界；超长无换行输入被截断并断开；首屏 payload 下降一个数量级；`node scripts/ui-kit-gateway-check.mjs`、`node scripts/webapp-check.mjs`、`cd ui/webapp/webui && npm run check:all` 全绿。

## 文件与接口

- `core/blitzkrieg_core/src/ipc/server/session.rs`：通道类型、写端超时、`MAX_LINE_BYTES` 读截断、读超时
- `scripts/ui-kit-gateway-check.mjs` / `scripts/webapp-check.mjs`：端到端验收
- `ui/ui_kit/src/web/gateway_run.rs`：`with_shared_gateway(client, 0, …)` 的 limit=0（全量）替换为分页限度

## 审批

本页先行提交，**与 K3 一并审批后再动代码**；审批前不在 `session.rs`、`gateway_run.rs`、两个 gate 脚本上做实现改动。
