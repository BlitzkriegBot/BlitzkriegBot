#![cfg(test)]

//! #1282 背压语义的 wire 级测试。这些行为只有对真实 socketpair 打满
//! 整个会话任务（读循环 + 通知任务 + 写任务）才观察得到，handle_line
//! 级的 `rpc` 助手够不着这些任务。

use super::*;

use crate::ipc::server::session::{MAX_LINE_BYTES, spawn_session};
use crate::service::{Core, CoreConfig};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// 打满 spawn_session 的最小 fixture：core + 事件 bus + 一对 tokio
/// UnixStream（服务端一端进 spawn_session，客户端一端留在测试手里）。
/// 数据面全关、无插件——背压语义不依赖任何挂载。
async fn session_fixture() -> (UnixStream, broadcast::Sender<Event>) {
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let core = Arc::new(AsyncMutex::new(Core::new(cfg)));
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
    let (bus_tx, _) = broadcast::channel::<Event>(512);
    // socketpair：std 一对 → 双端各包一层 tokio。服务端一端交给
    // spawn_session，客户端一端留在测试里读写。
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    ours.set_nonblocking(true).expect("nonblocking client");
    theirs.set_nonblocking(true).expect("nonblocking server");
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
    (client, bus_tx)
}

/// 收满 `lines_wanted` 个换行（带超时兜底，防测试挂死），返回累积字节
/// 与是否收满。
async fn read_lines<R>(client: &mut R, lines_wanted: usize, budget: Duration) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut acc = Vec::new();
    let mut buf = [0u8; 8192];
    let mut lines = 0usize;
    let mut done = false;
    let _ = tokio::time::timeout(budget, async {
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

/// P1-12b 的回归钉子：一次 read() 带出的多行必须全部按序处理。三个
/// 请求拼成一次 write 到达服务端——正是当年自定义循环丢第 3 个请求
/// 的形状；三条响应必须按序返回、无丢失。
#[tokio::test]
async fn one_read_carrying_three_lines_yields_three_responses_in_order() {
    let (mut client, _bus) = session_fixture().await;
    // 单次 write_all = 单个 socket 段：服务端第一次 read 就会整体读到。
    let payload = format!("{}\n{}\n{}\n", stats_line(1), stats_line(2), stats_line(3));
    client.write_all(payload.as_bytes()).await.expect("write");
    let (acc, done) = read_lines(&mut client, 3, Duration::from_secs(8)).await;
    assert!(done, "all 3 responses must arrive");
    let text = String::from_utf8(acc).expect("responses are utf-8");
    let ids = response_ids(&text);
    assert_eq!(
        ids,
        vec![1, 2, 3],
        "all 3 lines from ONE read must be answered in order"
    );
}

/// 契约 3：超长无换行输入必须截断并断开，而不是无限缓冲单行。1MB+1KB
/// 的 'x' 流过去之后，会话必须收场（客户端读到 EOF/连接错误）。
#[tokio::test]
async fn an_oversized_line_without_newline_disconnects_instead_of_buffering() {
    let (mut client, _bus) = session_fixture().await;
    // 1MB + 1KB：服务端在读循环里按 8KB 块累积，pending 超过
    // MAX_LINE_BYTES 即截断断开。断开前它会消费 >1MB，客户端的
    // write 不会挂死（余量 <1KB，进缓冲或直接 EPIPE 都算断开面）。
    let blob = vec![b'x'; MAX_LINE_BYTES + 1024];
    let write = tokio::time::timeout(Duration::from_secs(8), client.write_all(&blob)).await;
    assert!(
        write.is_ok(),
        "the server must keep consuming while it accumulates the oversized line"
    );
    // 会话已断：读到 EOF（Ok(0)）或连接错误都算「断开」。
    let mut buf = [0u8; 64];
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the session must disconnect on an oversized newline-less line"
    );
}

/// 契约 1 的丢通知腿：写端停滞（客户端不读、socket 缓冲已满）、响应
/// 队列灌满时，通知必须被 try_send 丢弃且通知任务不阻塞；此后响应
/// 路径不丢——全部 4000 条响应最终按序到达，且被丢的通知绝不迟到。
#[tokio::test]
async fn a_stalled_writer_drops_notifications_without_losing_responses() {
    let (client, bus) = session_fixture().await;
    // 4000 个请求 > 队列容量 1024 + socket 缓冲可吸收的条数：客户端
    // 不读，写端必然卡死在 socket 上，队列灌满后读循环卡在响应 send
    // 上——此刻 broadcast 通知只能被丢弃（这正是要钉住的语义）。
    const FLOOD: u64 = 4000;
    let mut flood = String::new();
    for id in 1..=FLOOD {
        flood.push_str(&stats_line(id as u32));
        flood.push('\n');
    }
    // 写半端放后台：队列灌满后服务端停止读请求，写会跟着卡住——卡住
    // 没关系，客户端并发读响应后自然续上（不能先 join 写任务再读：
    // 服务端在等客户端读、客户端在等写任务收尾，三方互等死锁）。
    let (mut client_read, client_write) = client.into_split();
    let flood_writer = tokio::spawn(async move {
        let mut client_write = client_write;
        client_write
            .write_all(flood.as_bytes())
            .await
            .expect("flood write must complete once the session drains");
    });
    // 开始收：全部响应按序到达，且没有任何一条通知混进来。
    // 给服务端时间把队列灌满（写端停滞、读循环卡在响应 send 上）。
    tokio::time::sleep(Duration::from_millis(500)).await;
    bus.send(Event::Ready {
        version: "test".into(),
        mode: Mode::Dry,
    })
    .expect("bus must stay alive");
    tokio::time::sleep(Duration::from_millis(200)).await;
    // 契约：全部 4000 条响应「最终」按序到达。会话流里还夹着服务端
    // 广播的 Ready 通知（bus.send 上方），主机抖动下它可能先于最后一
    // 条响应到达——按「带 id 的响应行」计数，不按总行数截断，避免
    // 把一条通知当成响应而把响应 4000 挤出窗口。总预算 5 分钟（并发
    // 门禁满载全部核时调度抖动可很大），单次读 30 s。
    let mut ids: Vec<u64> = Vec::with_capacity(FLOOD as usize);
    let mut response_text = Vec::new();
    let start = Instant::now();
    let hard_cap = Duration::from_secs(300);
    while ids.len() < FLOOD as usize && start.elapsed() < hard_cap {
        let (more, _done) = read_lines(&mut client_read, 4096, Duration::from_secs(30)).await;
        if more.is_empty() {
            break;
        }
        for line in more.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line)
                && let Some(id) = v.get("id").and_then(|v| v.as_u64())
            {
                ids.push(id);
                response_text.extend_from_slice(line);
                response_text.push(b'\n');
            }
        }
    }
    assert!(
        ids.len() == FLOOD as usize,
        "expected {FLOOD} responses within budget, got {}",
        ids.len()
    );
    assert_eq!(
        ids,
        (1..=FLOOD).collect::<Vec<u64>>(),
        "the response path must lose nothing and stay in order"
    );
    let text = String::from_utf8(response_text).expect("responses are utf-8");
    // 响应路径里不得掺入通知：被满队列丢弃的通知不得晚到并伪装成响应。
    assert!(
        !text.contains("\"core.event\""),
        "a notification dropped against a full queue must never surface late"
    );
    flood_writer.await.expect("flood writer must not panic");
    let ids = response_ids(&text);
    let expected: Vec<u64> = (1..=FLOOD).collect();
    assert_eq!(
        ids, expected,
        "the response path must lose nothing and stay in order"
    );
    assert!(
        !text.contains("\"core.event\""),
        "a notification dropped against a full queue must never surface late"
    );
}

/// 契约 1 的响应腿（慢写端）：客户端停滞但仍活着（远小于 WRITE_STALL），
/// 响应在有界队列里排队等候——之后慢慢收，全部响应必须按序到达，
/// 一条不丢（unbounded 时代这个窗口是内存无限增长面）。
#[tokio::test]
async fn a_slow_but_alive_writer_loses_no_response_within_write_stall() {
    let (mut client, _bus) = session_fixture().await;
    // 600 < OUTBOUND_QUEUE_CAP(1024)：队列装得下全部响应，写端停滞
    // 期间既不丢通知也不丢响应，只是排队。
    const N: u64 = 600;
    let mut flood = String::new();
    for id in 1..=N {
        flood.push_str(&stats_line(id as u32));
        flood.push('\n');
    }
    client
        .write_all(flood.as_bytes())
        .await
        .expect("flood write");
    // 写端停滞窗口：客户端 300ms 不读，socket 缓冲填满、写任务卡住、
    // 响应在队列里排队——但都远在 WRITE_STALL 之内。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (acc, done) = read_lines(&mut client, N as usize, Duration::from_secs(20)).await;
    assert!(
        done,
        "a slow writer within WRITE_STALL must not get the session cut; got {} lines",
        acc.iter().filter(|&&b| b == b'\n').count()
    );
    let text = String::from_utf8(acc).expect("responses are utf-8");
    let ids = response_ids(&text);
    let expected: Vec<u64> = (1..=N).collect();
    assert_eq!(
        ids, expected,
        "a slow-but-alive client gets every response, in order"
    );
}
