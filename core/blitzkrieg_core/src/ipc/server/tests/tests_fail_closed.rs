#![cfg(test)]

//! issue #46：IPC session 关键路径 fail-closed（wire 级，对真 socketpair 的
//! 完整会话任务读循环 + 通知任务 + 写任务才能观察）。
//!
//! - 写失败不得静默成功：对端消失后，会话的响应写入必失败并收场，不得假装
//!   写入成功，也不得拖累内核或其他会话；
//! - 连接中断不得丢失已入出站队列的响应：响应走 `send`（backpressure、WRITE_STALL
//!   超时后才放弃）而非 `try_send` 丢弃，已排队的响应必须按序送达；
//! - 读失败不得返回 stale 数据：读 EOF / 非法行即让会话收场，绝不编造响应行。

use super::*;
use crate::ipc::server::session::spawn_session;
use crate::service::{Core, CoreConfig};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// 最小会话 fixture：core + 事件 bus + 一对 tokio UnixStream（服务端一端进
/// spawn_session，客户端一端留在测试里读写）。不依赖任何数据面挂载，与
/// `tests_backpressure` 同构但不跨模块引用私有项。
async fn core_and_bus() -> (Arc<AsyncMutex<Core>>, broadcast::Sender<Event>) {
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let core = Arc::new(AsyncMutex::new(Core::new(cfg)));
    let bus_tx = broadcast::channel::<Event>(512).0;
    (core, bus_tx)
}

/// 在给定 core/bus 上 spawn 一条 session，返回客户端句柄。
async fn spawn_on(core: Arc<AsyncMutex<Core>>, bus_tx: &broadcast::Sender<Event>) -> UnixStream {
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: false,
        discovery_enabled: false,
        discovery_assets: Vec::new(),
        discovery_round_duration_sec: 0,
        discovery_mounted: Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        feed_reuse: None,
        last_dropped_batches: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::BTreeMap::new(),
        )),
    };
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    ours.set_nonblocking(true).expect("nonblocking");
    theirs.set_nonblocking(true).expect("nonblocking");
    let server_end = UnixStream::from_std(theirs).expect("tokio server end");
    let client = UnixStream::from_std(ours).expect("tokio client end");
    spawn_session(
        server_end,
        PeerAuth::SameUid { uid: own_uid() },
        core,
        bus_tx.subscribe(),
        registry,
        update_state,
        jobs,
        host,
        data_plane,
    );
    client
}

/// 收满 `lines_wanted` 个换行（带超时兜底，防测试挂死）。
async fn read_lines(client: &mut UnixStream, lines_wanted: usize) -> (Vec<u8>, bool) {
    let mut acc = Vec::new();
    let mut buf = [0u8; 8192];
    let mut lines = 0usize;
    let mut done = false;
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while lines < lines_wanted {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    lines += buf[..n].iter().filter(|&&b| b == b'\n').count();
                    acc.extend_from_slice(&buf[..n]);
                }
            }
        }
        done = lines >= lines_wanted;
    })
    .await;
    (acc, done)
}

/// 从 wire 文本抽出响应 id 序列（每行一个 JSON 对象）。
fn response_ids(text: &str) -> Vec<u64> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v.get("id").and_then(Value::as_u64))
        .collect()
}

/// 写一条 stats_line 请求（带换行）。
async fn stats_request(client: &mut UnixStream, id: u32) {
    client
        .write_all(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"engine.stats\",\"params\":{{}}}}\n"
            )
            .as_bytes(),
        )
        .await
        .expect("write request");
}

// ---------------------------------------------------------------------------
// issue #46 三条验收：写失败、连接中断、读失败
// ---------------------------------------------------------------------------

/// 写失败不得静默成功。客户端整体断开后，服务端向该会话的写入必失败，
/// 会话必须收场；失败只终结当前会话，不得拖死内核——同一 core 上的新
/// 会话仍须在预算内正常问答（fail-closed，不静默成功，不拖累其他会话）。
#[tokio::test]
async fn session_write_failure_is_surfaced_as_teardown_not_silent_success() {
    let (core, bus) = core_and_bus().await;
    let mut client = spawn_on(core.clone(), &bus).await;

    // 会话一开始必须正常：一个请求拿到带正确 id 的响应。
    stats_request(&mut client, 1).await;
    let (acc, done) = read_lines(&mut client, 1).await;
    assert!(
        done,
        "首条响应必须送达，got {} lines",
        acc.iter().filter(|&&b| b == b'\n').count()
    );
    let text = String::from_utf8(acc).expect("responses are utf-8");
    assert!(text.contains("\"id\":1"), "响应必须携带请求 id");

    // 客户端整体断开：服务端下一次向该会话的写入（以及读）必失败。
    drop(client);

    // 写失败不得拖死内核：同一 core 上新会话仍能正常问答。
    let mut client2 = spawn_on(core, &bus).await;
    stats_request(&mut client2, 2).await;
    let (acc2, done2) = read_lines(&mut client2, 1).await;
    assert!(
        done2,
        "写失败的会话不得拖死内核：新会话必须能拿到响应，got {} lines",
        acc2.iter().filter(|&&b| b == b'\n').count()
    );
    let text2 = String::from_utf8(acc2).expect("responses are utf-8");
    assert!(text2.contains("\"id\":2"), "新会话响应必须携带正确 id");
}

/// 连接中断不得丢失已入出站队列的响应。客户端读完全部响应后断开，在
/// 中断前每一条排队响应都必须按序送达（响应走 send/backpressure，决不
/// try_send 静默丢弃；那才是通知的策略，见 session.rs 的 NOTIFY_DROP 逻辑）。
#[tokio::test]
async fn session_connection_break_loses_no_response_queued_before_teardown() {
    let (core, bus) = core_and_bus().await;
    let mut client = spawn_on(core, &bus).await;

    const N: u64 = 600; // < OUTBOUND_QUEUE_CAP(1024)，全部能入队
    let mut flood = String::new();
    for id in 1..=N {
        flood.push_str(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"engine.stats\",\"params\":{{}}}}"
        ));
        flood.push('\n');
    }
    client
        .write_all(flood.as_bytes())
        .await
        .expect("flood write");

    // 全部响应必须按序到达，一条不丢——之后客户端再断开也不得让已入队
    // 的响应丢失。
    let (acc, done) = read_lines(&mut client, N as usize).await;
    assert!(
        done,
        "必须收到全部 {N} 条响应，got {} lines / {} bytes",
        acc.iter().filter(|&&b| b == b'\n').count(),
        acc.len()
    );
    let text = String::from_utf8(acc).expect("responses are utf-8");
    let ids = response_ids(&text);
    assert_eq!(
        ids,
        (1..=N).collect::<Vec<_>>(),
        "响应必须按序送达、不丢失、不重复"
    );
    assert!(!text.contains("\"core.event\""), "通知不得混入响应流");
}

/// 读失败不得返回 stale 数据。合法请求得合法响应；随后发非法行（无效
/// UTF-8），读失败必须让会话收场（EOF），绝不为那条行编造任何响应
/// 串（更不得是 stale/兜底数据）。
#[tokio::test]
async fn session_read_failure_fabricates_no_stale_response() {
    let (core, bus) = core_and_bus().await;
    let mut client = spawn_on(core, &bus).await;

    // 合法请求 → 合法响应，会话正常。
    stats_request(&mut client, 1).await;
    let (acc, done) = read_lines(&mut client, 1).await;
    assert!(
        done,
        "合法请求必须拿到响应，got {} lines",
        acc.iter().filter(|&&b| b == b'\n').count()
    );
    let text = String::from_utf8(acc).expect("responses are utf-8");
    assert!(text.contains("\"id\":1"), "响应必须携带请求 id");

    // 非法行：无效 UTF-8 以换行结尾。服务端读到后必须收场，不回任何
    // 响应行给那条非法输入。
    client
        .write_all(b"\xff\xfe\x00bad line\n")
        .await
        .expect("write invalid line");

    let mut buf = [0u8; 64];
    let mut extra_lines = 0usize;
    let eof = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => extra_lines += buf[..n].iter().filter(|&&b| b == b'\n').count(),
            }
        }
    })
    .await;
    assert!(
        eof.is_ok(),
        "读失败后会话必须收场（客户端读到 EOF/连接错误）"
    );
    assert_eq!(
        extra_lines, 0,
        "读失败不得返回任何响应行（fail-closed，不编造 stale 数据）"
    );
}
