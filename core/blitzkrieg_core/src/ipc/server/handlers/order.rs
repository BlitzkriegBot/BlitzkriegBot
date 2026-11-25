use super::super::{SessionState, core_err, effective_default_account, typed};
use super::prelude::*;
use crate::schema::*;

// The `order` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_order(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    session: &mut SessionState,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::ORDER_PLACE => {
            // §9.5: an order with NO accountId of its own spends this
            // connection's default — the SESSION's active account (what
            // `account.switch` set), else the process-level one. The check is
            // on the RAW params, BEFORE the serde default fills the gap: an
            // omitted accountId is "this connection's default", but an
            // EXPLICIT `accountId: "default"` NAMES that account and must
            // stand — an explicit id is never re-routed by a session cell.
            let explicit_account = params.get("accountId").is_some();
            typed(params, |p: PlaceParams| {
                let core = core.clone();
                async move {
                    let mut c = core.lock().await;
                    let mut order = p.order;
                    if !explicit_account {
                        // issue 561: 缺省解析走统一 helper（session → display
                        // → 进程 default），display 是面板唯一可信账户源。
                        order.account_id = effective_default_account(session, c.accounts());
                    }
                    // `place_outcome`, not `place`: when the kernel refuses the leg
                    // itself (a dry taker the book cannot fill) the result carries
                    // the structured reason instead of a bare REJECTED (#180).
                    let outcome = c.place_outcome(order, p.maker_timeout_ms, now_ms())?;
                    let (code, message) = match outcome.rejection {
                        Some(err) => (Some(err.code), Some(err.message)),
                        None => (None, None),
                    };
                    Ok::<_, CoreError>(
                        serde_json::to_value(PlaceResult {
                            order_id: outcome.order_id,
                            status: outcome.status,
                            code,
                            message,
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        method::ORDER_CANCEL => {
            typed(params, |p: CancelParams| {
                let core = core.clone();
                async move {
                    core.lock().await.cancel(&p.order_id, now_ms())?;
                    Ok::<_, CoreError>(serde_json::json!({ "success": true }))
                }
            })
            .await
        }
        method::ORDER_CANCEL_ALL => {
            let p: CancelAllParams = serde_json::from_value(params.clone()).unwrap_or_default();
            match core
                .lock()
                .await
                .cancel_all(p.token_id.as_deref(), now_ms())
            {
                Ok(n) => {
                    Ok(serde_json::to_value(CancelAllResult { cancelled: n })
                        .unwrap_or(Value::Null))
                }
                Err(e) => Err(core_err(e)),
            }
        }
        method::ORDER_LIST => {
            // #186: the tracked-order table only grows, so the reply is a page.
            // `total` rides along because a bare `orders` array cannot say
            // whether it is the whole table or a truncated window — and the
            // caller's job here is to find ghost orders, which a silent
            // truncation would hide.
            //
            // Absent / 0 `limit` = the default page (not "everything": the
            // wire never gets to ask for an unbounded read). An explicit large
            // `limit` is honoured up to `ORDERS_DEFAULT_LIMIT` so nothing that
            // worked before can ask for more than the default and quietly get
            // less; `offset` pages back from the newest end.
            let limit = match params.get("limit").and_then(|v| v.as_u64()) {
                None | Some(0) => crate::service::ORDERS_DEFAULT_LIMIT,
                Some(n) => (n as usize).min(crate::service::ORDERS_DEFAULT_LIMIT),
            };
            let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let (orders, total) = core.lock().await.list_orders_window(limit, offset);
            Ok(serde_json::json!({ "orders": orders, "total": total }))
        }
        method::ORDER_CANCEL_REMAINING => {
            typed(params, |p: CancelRemainingParams| {
                let core = core.clone();
                async move {
                    core.lock().await.cancel_remaining(&p.order_id, now_ms())?;
                    Ok::<_, CoreError>(serde_json::json!({ "success": true }))
                }
            })
            .await
        }
        method::ORDER_FORCE_CLOSE => {
            typed(params, |p: ForceCloseParams| {
                let core = core.clone();
                async move {
                    core.lock().await.force_close_order(
                        &p.order_id,
                        now_ms(),
                        "manual orders.force_close",
                    )?;
                    Ok::<_, CoreError>(serde_json::json!({ "success": true }))
                }
            })
            .await
        }
        method::ORDER_CLOSE_FILLED => {
            typed(params, |p: CloseFilledParams| {
                let core = core.clone();
                async move {
                    let closed = core.lock().await.close_filled(&p.order_id, now_ms())?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(CloseFilledResult { closed }).unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        method::ORDER_RECONCILE => {
            typed(params, |p: ReconcileParams| {
                let core = core.clone();
                async move {
                    let snap = crate::reconcile::VenueSnapshot {
                        open_order_ids: p.open_order_ids,
                        trades: p
                            .trades
                            .into_iter()
                            .map(|t| crate::reconcile::VenueTrade {
                                venue_order_id: t.venue_order_id,
                                trade_id: t.trade_id,
                                token_id: t.token_id,
                                side: t.side,
                                size: t.size,
                                price: t.price,
                                ts_ms: t.ts_ms,
                                tx_hash: None,
                                maker: t.maker,
                                // #175: passed through verbatim. `None` (the
                                // caller could not prove the owner) parks the
                                // trade — the core never fills this blank in.
                                account: t.account,
                            })
                            .collect(),
                        now_ms: now_ms(),
                    };
                    let r = core.lock().await.reconcile(snap)?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(ReconcileResultView {
                            filled: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. })
                                })
                                .count(),
                            marked_filled: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(
                                        a,
                                        crate::reconcile::ReconcileAction::MarkedFilled { .. }
                                    )
                                })
                                .count(),
                            marked_cancelled: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(
                                        a,
                                        crate::reconcile::ReconcileAction::MarkedCancelled { .. }
                                    )
                                })
                                .count(),
                            marked_force_closed: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(
                                        a,
                                        crate::reconcile::ReconcileAction::MarkedForceClosed { .. }
                                    )
                                })
                                .count(),
                            ghost_ids: r.suspect_ghost_ids,
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
