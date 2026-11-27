use super::prelude::*;
use crate::market::registry::MarketPluginRegistry;
use crate::schema::*;
use rust_decimal::Decimal;

/// Default supported round durations when a market plugin does not declare a
/// narrower list (e.g., Polymarket supports 5m, 15m, 1h, daily).
const DEFAULT_ROUND_DURATIONS: &[i64] = &[300, 900, 3600, 86400];

fn format_duration_label(sec: i64) -> String {
    match sec {
        300 => "5m".to_string(),
        900 => "15m".to_string(),
        3600 => "1h".to_string(),
        14400 => "4h".to_string(),
        86400 => "daily".to_string(),
        other if other % 60 == 0 => format!("{}m", other / 60),
        other => format!("{other}s"),
    }
}

/// Resolve each venue's supported round durations and their sorted intersection.
/// Returns `(common, leg_a_supported, leg_b_supported, locked_by_venue)`.
fn resolve_common_round_durations(
    registry: &MarketPluginRegistry,
    allowed_union: &[i64],
    leg_a_venue: &str,
    leg_b_venue: &str,
) -> (Vec<i64>, Vec<i64>, Vec<i64>, Option<String>) {
    let fallback: Vec<i64> = if allowed_union.is_empty() {
        DEFAULT_ROUND_DURATIONS.to_vec()
    } else {
        let mut v = allowed_union.to_vec();
        v.sort_unstable();
        v.dedup();
        v
    };
    let venue_rounds = |venue: &str| -> Vec<i64> {
        let mut v = registry
            .get(venue.trim())
            .and_then(|p| p.supported_round_secs())
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| fallback.clone());
        v.sort_unstable();
        v.dedup();
        v
    };
    let a = venue_rounds(leg_a_venue);
    let b = venue_rounds(leg_b_venue);
    let common: Vec<i64> = a.iter().copied().filter(|d| b.contains(d)).collect();
    let locked_by = if common.len() == 1 {
        if b.len() == 1 && a.len() > 1 {
            Some(leg_b_venue.trim().to_string())
        } else if a.len() == 1 && b.len() > 1 {
            Some(leg_a_venue.trim().to_string())
        } else {
            None
        }
    } else {
        None
    };
    (common, a, b, locked_by)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_arb(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    registry: &MarketPluginRegistry,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::ARB_CONFIG_SET => {
            let params: crate::schema::ArbConfigSetParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return Err((
                        Failure::PARSE_ERROR,
                        format!("invalid arb.config.set params: {e}"),
                        None,
                    ));
                }
            };
            if params.version != 1 {
                return Err((
                    Failure::APPLICATION,
                    format!(
                        "arb.config.set: `version` must be 1 (got {})",
                        params.version
                    ),
                    None,
                ));
            }
            let mut c = core.lock().await;
            let (common, leg_a_rounds, leg_b_rounds, locked_by) = resolve_common_round_durations(
                registry,
                &c.config().allowed_round_secs,
                &params.leg_a_venue,
                &params.leg_b_venue,
            );
            if common.is_empty() {
                return Err((
                    Failure::APPLICATION,
                    format!(
                        "arb.config.set: venues `{}` ({:?}) and `{}` ({:?}) share no common round duration",
                        params.leg_a_venue, leg_a_rounds, params.leg_b_venue, leg_b_rounds
                    ),
                    None,
                ));
            }
            let resolved_round_sec = match params.round_duration_sec.filter(|d| *d > 0) {
                Some(requested) => {
                    if !common.contains(&requested) {
                        return Err((
                            Failure::APPLICATION,
                            format!(
                                "arb.config.set: roundDurationSec={requested}s is not supported by both `{}` and `{}` (common: {:?})",
                                params.leg_a_venue, params.leg_b_venue, common
                            ),
                            None,
                        ));
                    }
                    Some(requested)
                }
                None => {
                    if common.len() == 1 {
                        Some(common[0])
                    } else if common.contains(&900) {
                        Some(900)
                    } else {
                        common.first().copied()
                    }
                }
            };
            let cfg = crate::account::CrossVenueArbConfig {
                leg_a_venue: params.leg_a_venue,
                leg_a_account: params.leg_a_account,
                leg_b_venue: params.leg_b_venue,
                leg_b_account: params.leg_b_account,
                pairs: params.pairs,
                round_duration_sec: resolved_round_sec,
            };
            let actor = params
                .actor
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("panel")
                .to_string();
            match c.arb_config_set_persisting(&cfg, &actor, "arb.config.set", now_ms()) {
                Ok(()) => Ok(serde_json::json!({
                    "version": crate::schema::PROTOCOL_VERSION,
                    "configured": true,
                    "roundDurationSec": resolved_round_sec,
                    "commonRoundDurations": common,
                    "lockedByVenue": locked_by,
                })),
                Err(e) => Err((Failure::APPLICATION, e.to_string(), None)),
            }
        }
        method::ARB_CONFIG_GET => {
            let c = core.lock().await;
            let payload = match c.arb_leg_config() {
                Some(cfg) => {
                    let (common, leg_a_rounds, leg_b_rounds, locked_by) =
                        resolve_common_round_durations(
                            registry,
                            &c.config().allowed_round_secs,
                            &cfg.leg_a_venue,
                            &cfg.leg_b_venue,
                        );
                    let effective_round_sec =
                        cfg.round_duration_sec.filter(|d| *d > 0).or_else(|| {
                            if common.len() == 1 {
                                Some(common[0])
                            } else {
                                None
                            }
                        });
                    serde_json::json!({
                        "version": crate::schema::PROTOCOL_VERSION,
                        "configured": true,
                        "legAVenue": cfg.leg_a_venue,
                        "legAAccount": cfg.leg_a_account,
                        "legBVenue": cfg.leg_b_venue,
                        "legBAccount": cfg.leg_b_account,
                        "pairs": cfg.pairs,
                        "roundDurationSec": effective_round_sec,
                        "commonRoundDurations": common,
                        "legASupportedRounds": leg_a_rounds,
                        "legBSupportedRounds": leg_b_rounds,
                        "lockedByVenue": locked_by,
                    })
                }
                None => serde_json::json!({
                    "version": crate::schema::PROTOCOL_VERSION,
                    "configured": false,
                    "legAVenue": null,
                    "legAAccount": null,
                    "legBVenue": null,
                    "legBAccount": null,
                    "pairs": [],
                    "roundDurationSec": null,
                    "commonRoundDurations": [],
                    "legASupportedRounds": [],
                    "legBSupportedRounds": [],
                    "lockedByVenue": null,
                }),
            };
            Ok(payload)
        }
        method::ARB_CONFIG_CLEAR => {
            let actor = params
                .get("actor")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("panel")
                .to_string();
            let mut c = core.lock().await;
            match c.arb_config_clear_persisting(&actor) {
                Ok(()) => Ok(serde_json::json!({
                    "version": crate::schema::PROTOCOL_VERSION,
                    "configured": false,
                })),
                Err(e) => Err((Failure::APPLICATION, e.to_string(), None)),
            }
        }
        method::ARB_STATUS => {
            let c = core.lock().await;
            let now = now_ms();
            let cfg_opt = c.arb_leg_config().cloned();
            let running = c.engine().is_some_and(|e| {
                e.enabled_strategies()
                    .iter()
                    .any(|s| s == crate::service::ARB_STRATEGY)
            });

            let (config_json, readiness, scanner_rows) = match &cfg_opt {
                Some(cfg) => {
                    let (common, leg_a_rounds, leg_b_rounds, locked_by) =
                        resolve_common_round_durations(
                            registry,
                            &c.config().allowed_round_secs,
                            &cfg.leg_a_venue,
                            &cfg.leg_b_venue,
                        );
                    let effective_round_sec =
                        cfg.round_duration_sec.filter(|d| *d > 0).or_else(|| {
                            if common.len() == 1 {
                                Some(common[0])
                            } else {
                                None
                            }
                        });
                    let mut effective_cfg = cfg.clone();
                    effective_cfg.round_duration_sec = effective_round_sec;
                    let scanner = c
                        .engine()
                        .map(|e| e.arb_scanner_snapshot(&effective_cfg, now))
                        .unwrap_or_default();

                    let acc_a = c
                        .accounts()
                        .account(&crate::account::AccountId::from(cfg.leg_a_account.as_str()));
                    let acc_b = c
                        .accounts()
                        .account(&crate::account::AccountId::from(cfg.leg_b_account.as_str()));
                    let a_trading = acc_a.is_some_and(|a| a.trading_enabled);
                    let b_trading = acc_b.is_some_and(|a| a.trading_enabled);
                    let a_frozen = c
                        .account_gate(&cfg.leg_a_account)
                        .is_some_and(|g| g.blocked);
                    let b_frozen = c
                        .account_gate(&cfg.leg_b_account)
                        .is_some_and(|g| g.blocked);

                    let paired_books_ready = scanner
                        .iter()
                        .filter(|row| {
                            row["legA"]["marketFound"] == serde_json::json!(true)
                                && row["legB"]["marketFound"] == serde_json::json!(true)
                                && row["legA"]["stale"] == serde_json::json!(false)
                                && row["legB"]["stale"] == serde_json::json!(false)
                        })
                        .count();
                    let round_aligned = effective_round_sec.is_some_and(|d| common.contains(&d));
                    let round_detail = match (effective_round_sec, &locked_by) {
                        (Some(d), Some(v)) => format!(
                            "已锁定 {}（跟随 {} 唯一支持周期）",
                            format_duration_label(d),
                            v
                        ),
                        (Some(d), None) => {
                            format!("已对齐共同周期 {}", format_duration_label(d))
                        }
                        (None, _) => "未锁定共同轮次周期，请重新保存双腿配置".to_string(),
                    };
                    let checks = vec![
                        serde_json::json!({
                            "id": "config",
                            "label": "双腿配置",
                            "ok": true,
                            "detail": format!("{} ({}) ⇄ {} ({}) · {} 个币对", cfg.leg_a_account, cfg.leg_a_venue, cfg.leg_b_account, cfg.leg_b_venue, cfg.pairs.len()),
                        }),
                        serde_json::json!({
                            "id": "round",
                            "label": "共同轮次周期",
                            "ok": round_aligned,
                            "detail": round_detail,
                        }),
                        serde_json::json!({
                            "id": "leg_a",
                            "label": format!("A 腿账户 ({})", cfg.leg_a_account),
                            "ok": a_trading && !a_frozen,
                            "detail": if a_frozen {
                                "账户已被风控冻结"
                            } else if !a_trading {
                                "账户交易开关未开启"
                            } else {
                                "交易开启且未冻结"
                            },
                        }),
                        serde_json::json!({
                            "id": "leg_b",
                            "label": format!("B 腿账户 ({})", cfg.leg_b_account),
                            "ok": b_trading && !b_frozen,
                            "detail": if b_frozen {
                                "账户已被风控冻结"
                            } else if !b_trading {
                                "账户交易开关未开启"
                            } else {
                                "交易开启且未冻结"
                            },
                        }),
                        serde_json::json!({
                            "id": "engine",
                            "label": "套利引擎开关",
                            "ok": running,
                            "detail": if running { "cross_venue_arb 已武装运行中" } else { "策略当前已暂停，打开顶部开关即可扫盘" },
                        }),
                        serde_json::json!({
                            "id": "books",
                            "label": "双腿实时盘口",
                            "ok": paired_books_ready > 0,
                            "detail": if paired_books_ready > 0 {
                                format!("{paired_books_ready}/{} 个监控币对双腿盘口实时在线", cfg.pairs.len())
                            } else {
                                "暂未同时收到 A/B 两侧新鲜盘口（请检查双腿交易所行情插件）".to_string()
                            },
                        }),
                    ];
                    let cfg_val = serde_json::json!({
                        "legAVenue": cfg.leg_a_venue,
                        "legAAccount": cfg.leg_a_account,
                        "legBVenue": cfg.leg_b_venue,
                        "legBAccount": cfg.leg_b_account,
                        "pairs": cfg.pairs,
                        "roundDurationSec": effective_round_sec,
                        "commonRoundDurations": common,
                        "legASupportedRounds": leg_a_rounds,
                        "legBSupportedRounds": leg_b_rounds,
                        "lockedByVenue": locked_by,
                    });
                    (cfg_val, checks, scanner)
                }
                None => (
                    serde_json::Value::Null,
                    vec![serde_json::json!({
                        "id": "config",
                        "label": "双腿配置",
                        "ok": false,
                        "detail": "尚未配置跨所套利双腿，点击「配置套利」开始",
                    })],
                    Vec::new(),
                ),
            };

            let active_pairs: Vec<Value> = c
                .pair_book_all()
                .into_iter()
                .filter(|p| p.status.as_str() != "closed")
                .map(|p| {
                    let legs: Vec<Value> = p
                        .legs
                        .iter()
                        .map(|l| {
                            serde_json::json!({
                                "accountId": l.account_id,
                                "tokenId": l.token_id,
                                "internalKey": l.internal_key,
                                "declaredShares": l.declared_shares.to_string(),
                                "filledShares": l.filled_shares.to_string(),
                                "dead": l.dead,
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "id": p.id,
                        "strategy": p.owner_strategy,
                        "slot": p.slot,
                        "status": p.status.as_str(),
                        "nakedToken": p.naked_token(),
                        "createdMs": p.created_ms,
                        "legs": legs,
                    })
                })
                .collect();

            let mut arb_orders: Vec<Value> = c
                .list_orders()
                .into_iter()
                .filter(|o| o.strategy == crate::service::ARB_STRATEGY)
                .map(|o| {
                    serde_json::json!({
                        "orderId": o.order_id,
                        "accountId": o.account_id.as_str(),
                        "asset": o.asset,
                        "direction": o.direction,
                        "side": o.side.as_str(),
                        "price": o.price.to_string(),
                        "size": o.size.to_string(),
                        "filledSize": o.filled_size.to_string(),
                        "status": serde_json::to_value(o.status).unwrap_or_else(|_| serde_json::json!(format!("{:?}", o.status))),
                        "createdAtMs": o.updated_at_ms,
                        "roundSlot": o.round_slot,
                    })
                })
                .collect();
            arb_orders.sort_by(|a, b| {
                b["createdAtMs"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["createdAtMs"].as_i64().unwrap_or(0))
            });
            arb_orders.truncate(50);

            let mut net_pnl = Decimal::ZERO;
            let mut wins: u64 = 0;
            let mut losses: u64 = 0;
            let mut arb_trades: Vec<Value> = c
                .positions()
                .closed_positions()
                .iter()
                .filter(|t| t.strategy == crate::service::ARB_STRATEGY)
                .map(|t| {
                    net_pnl += t.net_pnl_usd;
                    if t.net_pnl_usd >= Decimal::ZERO {
                        wins += 1;
                    } else {
                        losses += 1;
                    }
                    serde_json::json!({
                        "id": t.id,
                        "accountId": t.account_id.as_str(),
                        "venue": t.venue,
                        "asset": t.asset,
                        "direction": t.direction.as_str(),
                        "entryPrice": t.entry_price.to_string(),
                        "exitPrice": t.exit_price.to_string(),
                        "shares": t.shares.to_string(),
                        "netPnlUsd": t.net_pnl_usd.to_string(),
                        "exitReason": format!("{:?}", t.exit_reason),
                        "enteredAtMs": t.entered_at_ms,
                        "exitedAtMs": t.exited_at_ms,
                    })
                })
                .collect();
            arb_trades.sort_by(|a, b| {
                b["exitedAtMs"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["exitedAtMs"].as_i64().unwrap_or(0))
            });
            let closed_total = arb_trades.len();
            arb_trades.truncate(100);

            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "configured": cfg_opt.is_some(),
                "running": running,
                "config": config_json,
                "readiness": readiness,
                "scanner": scanner_rows,
                "activePairs": active_pairs,
                "summary": {
                    "netPnlUsd": net_pnl.to_string(),
                    "wins": wins,
                    "losses": losses,
                    "closedTrades": closed_total,
                    "activePairs": active_pairs.len(),
                    "ordersCount": arb_orders.len(),
                },
                "orders": arb_orders,
                "trades": arb_trades,
            }))
        }
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
