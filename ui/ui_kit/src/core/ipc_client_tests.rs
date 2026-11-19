//! Spec for the `connected` flag: it means "a core answered", not "we hold a
//! socket object".
//!
//! The defect this pins was found by the E12(c) gateway gate, not by reasoning:
//! after the core was SIGKILLed, one snapshot still reported `connected: true`
//! with empty arrays, and the panel drew that as a running engine with no
//! positions. The cause is that `connect()` short-circuits while a handle
//! exists — a handle outliving its peer is invisible until a *call* is made, so
//! the flag has to come from the call, not from the handle.

use crate::core::ipc_client::IpcClient;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

/// A one-shot UDS server: answers `replies` requests then closes the session,
/// which is what a killed core looks like from the client's side.
fn serve_then_close(sock: PathBuf, replies: usize) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(&sock).expect("bind test socket");
    std::thread::spawn(move || {
        // One persistent session is what the client keeps, so the loop below
        // serves each request on the SAME stream until the budget runs out.
        if let Ok((stream, _)) = listener.accept() {
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            for _ in 0..replies {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let id: serde_json::Value = serde_json::from_str(line.trim())
                    .ok()
                    .and_then(|v: serde_json::Value| v.get("id").cloned())
                    .unwrap_or(serde_json::json!(1));
                let reply = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "version": "0.1.0", "mode": "dry" },
                });
                let mut out = reply.to_string();
                out.push('\n');
                if writer
                    .write_all(out.as_bytes())
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            // Drop the session without ceremony: this is the crash, and it must
            // be the *next call* that notices, not an earlier one.
        }
    })
}

fn unique_sock(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bk-ipc-{}-{}.sock", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn a_snapshot_reports_connected_only_while_the_core_answers() {
    let sock = unique_sock("snap");
    // Three replies: the first snapshot needs `core.ready` plus one state call
    // to reach the point where `connected` is set; the second snapshot needs a
    // `ready` attempt that hits a closed session.
    let server = serve_then_close(sock.clone(), 3);

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let first = client.snapshot(0);
    assert!(
        first.connected,
        "a core that answers must be reported as connected (lastError={:?})",
        first.last_error
    );
    assert!(
        first.ready.is_some(),
        "the handshake payload must ride along"
    );

    // Let the server finish its budget and drop the session.
    std::thread::sleep(std::time::Duration::from_millis(200));

    let second = client.snapshot(0);
    assert!(
        !second.connected,
        "a cached handle whose core is gone must NOT be reported as connected — \
         this is the exact false positive the E12(c) gate caught"
    );
    assert!(
        second.last_error.is_some(),
        "an unreachable core must come with a reason the panel can show"
    );
    assert!(
        second.balance.is_none() && second.stats.is_none(),
        "and no half-filled state may be presented as live"
    );

    server.join().ok();
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn connecting_to_a_missing_socket_is_not_connected() {
    // The other direction, and the one a read-only panel hits: nothing is
    // listening. `connect()` fails outright, so this must stay false.
    let sock = unique_sock("absent");
    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let s = client.snapshot(0);
    assert!(!s.connected);
    assert!(s.last_error.is_some());
}

#[test]
fn a_dead_session_is_dropped_so_the_next_call_reconnects() {
    // Recovery depends on this: once a call fails, the handle must be gone, or
    // a restarted core could never be reached through the same client (the flag
    // would recover but every subsequent call would keep writing to the corpse).
    let sock = unique_sock("reconnect");
    let _ = std::fs::remove_file(&sock);
    {
        let listener = UnixListener::bind(&sock).expect("bind");
        // Hold and close immediately: the client connects, then loses the peer.
        std::thread::spawn(move || {
            if let Ok((_s, _)) = listener.accept() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
    }

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    assert!(client.connect().is_ok(), "the first connect succeeds");
    assert!(client.is_connected());
    // This call meets the closed peer.
    let _ = client.ready();
    assert!(
        !client.is_connected(),
        "a failed call must drop the handle, or a restarted core is unreachable"
    );
    let _ = std::fs::remove_file(&sock);
}

/// Issue 476: with no request in flight the kernel keeps streaming
/// `core.event` notifications, and the response to the NEXT request sits
/// behind that backlog. The old client gave up after 64 skipped lines and the
/// panel reported a live core as offline ("too many interleaved
/// notifications"). The fix removed the cap — the guards are the socket read
/// timeout and EOF — so this fixture floods 100 notifications first and the
/// request must still be answered.
#[test]
fn a_request_behind_a_notification_backlog_is_still_answered() {
    let sock = unique_sock("backlog");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind test socket");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        // The flood: notifications have no `id`, exactly like the kernel's
        // `core.event` pushes. 100 > the old 64-line cap.
        for i in 0..100u32 {
            let note = serde_json::json!({
                "jsonrpc": "2.0", "method": "core.event",
                "params": { "kind": "flood", "seq": i },
            });
            let mut out = note.to_string();
            out.push('\n');
            writer
                .write_all(out.as_bytes())
                .and_then(|_| writer.flush())
                .expect("notify");
        }
        // Then answer the one request, echo its id like the kernel does.
        let mut line = String::new();
        reader.read_line(&mut line).expect("read request");
        let id: serde_json::Value = serde_json::from_str(line.trim())
            .ok()
            .and_then(|v: serde_json::Value| v.get("id").cloned())
            .unwrap_or(serde_json::json!(1));
        let reply = serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "version": "0.1.0", "mode": "dry" },
        });
        let mut out = reply.to_string();
        out.push('\n');
        writer
            .write_all(out.as_bytes())
            .and_then(|_| writer.flush())
            .expect("reply");
        // Keep the session open briefly so the client's read lands.
        std::thread::sleep(std::time::Duration::from_millis(200));
    });

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    client.connect().expect("connect");
    let ready = client
        .ready()
        .expect("a request behind a 100-notification backlog must still be answered (issue 476)");
    assert_eq!(ready.version, "0.1.0");

    server.join().ok();
    let _ = std::fs::remove_file(&sock);
}

// ── #1294: the trades surface must not degrade silently ──────────────────────

/// A method-aware one-shot server: every request is answered with whatever
/// `reply_for` returns for its method — the payload map only (`{"result": …}`
/// or `{"error": …}`), so a single snapshot's many calls can carry ONE
/// scripted trades payload while everything else answers a benign empty
/// result. This is the injection point the #1294 tests need — the shipped
/// `serve_then_close` replies one fixed shape to every method.
///
/// The listener binds SYNCHRONOUSLY in the caller's thread: a bind inside the
/// spawned thread races the client's first connect (ECONNREFUSED).
fn serve_scripted(
    sock: PathBuf,
    budget: usize,
    reply_for: impl Fn(&str) -> serde_json::Value + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(&sock).expect("bind test socket");
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            // Idle read timeout: the budget is a CAP, not a promise — once the
            // client stops asking, read_line must not park the thread (a
            // parked server makes the join at the end of the test hang).
            stream
                .set_read_timeout(Some(std::time::Duration::from_millis(400)))
                .expect("set read timeout");
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            for _ in 0..budget {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let req: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let method = req
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
                let mut reply = reply_for(&method);
                reply["jsonrpc"] = serde_json::json!("2.0");
                reply["id"] = id;
                let mut out = reply.to_string();
                out.push('\n');
                if writer
                    .write_all(out.as_bytes())
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            // Keep the session briefly so late calls land on a live socket.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    })
}

/// One kernel-shaped trade row (camelCase, JSON numbers — `TradeRecord`'s
/// `rename_all` shape, the only shape `TradeView` accepts).
fn trade_row(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "strategy": "spread_arb", "asset": "BTC", "direction": "up",
        "entryPrice": 0.40, "exitPrice": 0.60, "shares": 250.0,
        "netPnlUsd": 50.0, "netPnlPct": 50.0, "feesUsd": 0.0,
        "exitReason": "take_profit", "holdTimeSec": 12,
        "entryTime": 1700000000001i64, "exitTime": 1700000012001i64,
    })
}

/// The defect this pins (found by the #1282 offline stress test): ONE drifted
/// row — same payload, `entryPrice` renamed — used to fail the whole
/// `Vec<TradeView>` parse, and `unwrap_or_default` turned that into an EMPTY
/// history on a snapshot reporting `connected: true, last_error: null`. The
/// good rows must survive the bad one, and the skip must be visible.
#[test]
fn a_shape_drifted_trades_row_is_counted_and_the_good_rows_survive() {
    let sock = unique_sock("trades-drift");
    let server = serve_scripted(sock.clone(), 64, |method| {
        if method == "trades.history" {
            let mut drifted = trade_row("hft-3");
            // The single rename that used to erase the whole surface: the
            // required key disappears, so the row fails its parse outright.
            drifted
                .as_object_mut()
                .expect("row object")
                .remove("entryPrice");
            return serde_json::json!({
                "result": {
                    "version": "1",
                    "trades": [trade_row("hft-1"), trade_row("hft-2"), drifted],
                },
            });
        }
        serde_json::json!({ "result": { "version": "0.1.0", "mode": "dry" } })
    });

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let snap = client.snapshot(0);

    assert_eq!(
        snap.trades.len(),
        2,
        "the two well-formed rows must survive their drifted sibling"
    );
    let degrade = snap
        .trades_degrade
        .as_ref()
        .expect("a dropped row must travel as a degrade signal, never disappear");
    assert!(!degrade.unavailable, "the call itself answered");
    assert_eq!(degrade.skipped_rows, 1);
    assert!(
        degrade
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("entryPrice"),
        "the reason must name the field that drifted: {:?}",
        degrade.reason
    );

    server.join().ok();
    let _ = std::fs::remove_file(&sock);
}

/// A healthy payload keeps the historical behaviour byte-for-byte: the rows
/// parse, and `trades_degrade` stays absent (an older panel build renders
/// exactly as before).
#[test]
fn a_healthy_trades_payload_carries_no_degrade_signal() {
    let sock = unique_sock("trades-healthy");
    let server = serve_scripted(sock.clone(), 64, |method| {
        if method == "trades.history" {
            return serde_json::json!({
                "result": {
                    "version": "1",
                    "trades": [trade_row("hft-1"), trade_row("hft-2")],
                },
            });
        }
        serde_json::json!({ "result": { "version": "0.1.0", "mode": "dry" } })
    });

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let snap = client.snapshot(0);

    assert_eq!(snap.trades.len(), 2);
    assert!(
        snap.trades_degrade.is_none(),
        "a clean response must not raise a banner"
    );

    server.join().ok();
    let _ = std::fs::remove_file(&sock);
}

/// The other half: when the call itself fails (RPC error), the snapshot must
/// say the surface is UNAVAILABLE rather than presenting an empty ledger.
#[test]
fn a_failed_trades_call_marks_the_surface_unavailable() {
    let sock = unique_sock("trades-down");
    let server = serve_scripted(sock.clone(), 64, |method| {
        if method == "trades.history" {
            return serde_json::json!({
                "error": { "code": -32000, "message": "trade log unreadable" },
            });
        }
        serde_json::json!({ "result": { "version": "0.1.0", "mode": "dry" } })
    });

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let snap = client.snapshot(0);

    assert!(snap.trades.is_empty());
    let degrade = snap
        .trades_degrade
        .as_ref()
        .expect("an unreadable trade log must not read as an empty one");
    assert!(degrade.unavailable);
    assert!(
        degrade
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("trade log unreadable"),
        "the core's message must ride through: {:?}",
        degrade.reason
    );

    server.join().ok();
    let _ = std::fs::remove_file(&sock);
}
