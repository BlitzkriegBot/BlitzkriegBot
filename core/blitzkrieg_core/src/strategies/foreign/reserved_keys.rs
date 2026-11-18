//! §2.4 reserved-key seal: stop-loss / take-profit / max-hold sizing belong
//! to the kernel (exit policy / risk); an intent that carries one is warned
//! about under target `strategy` and the key is dropped — the entry itself is
//! never refused. A free function on purpose: the seal must be provable
//! without a loaded library.
//! Moved verbatim from `foreign.rs` (pure code motion).

use crate::signal::TradeSignal;
use crate::strategies::{StrategyCtx, StrategyExitIntent};
use rust_decimal::Decimal;
use std::str::FromStr;

/// The three §2.4 reserved keys.
const RESERVED_INTENT_KEYS: [&str; 3] = [
    "suggested_stop_loss",
    "suggested_take_profit",
    "suggested_max_hold_sec",
];

/// Which reserved keys are present on one intent object, in declaration order.
/// The one place a reserved key can be observed, so both the warn/drop path
/// and its test read from here.
pub(super) fn reserved_intent_keys(obj: &serde_json::Value) -> Vec<&'static str> {
    RESERVED_INTENT_KEYS
        .iter()
        .copied()
        .filter(|k| obj.get(*k).is_some())
        .collect()
}

pub(super) fn parse_breaks(v: &serde_json::Value) -> Vec<(String, Decimal)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    let token = b.get("token").and_then(|t| t.as_str())?;
                    let price = b
                        .get("broken_price")
                        .and_then(|p| p.as_str())
                        .and_then(|s| Decimal::from_str(s).ok())
                        .unwrap_or(Decimal::ZERO);
                    Some((token.to_string(), price))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Adopt a strategy's evaluate JSON: split it into exit intents, breaks and
/// entry candidates. A free function (not a method) on purpose — the §2.4
/// reserved-key seal (warn + drop, entry otherwise untouched) must be
/// provable without a dylib, and its inputs are exactly the strategy name,
/// the JSON value and the round context.
///
/// #642 (RFC 568): `pair_default` is the library's OPTIONAL
/// `bk_strategy_entry_pair` declaration. An entry's own `"pair":{"key","leg"}`
/// JSON field wins over it; an entry with neither carries `pair: None` — the
/// pre-existing behaviour bit-for-bit. A present-but-malformed per-entry
/// `pair` degrades to the same fallback chain the `shares` field already uses
/// (never reject the entry), so a typo cannot cost a signal.
pub(super) fn adopt_eval_json(
    strategy_name: &str,
    scope: &str,
    v: serde_json::Value,
    ctx: &StrategyCtx<'_>,
    exit_intents: &mut Vec<StrategyExitIntent>,
    breaks_out: &mut Vec<(String, Decimal)>,
    pair_default: Option<&crate::signal::TradePairDecl>,
) -> Vec<TradeSignal> {
    if let Some(serde_json::Value::Array(exits)) = v.get("exits") {
        for e in exits {
            if let Some(token) = e.get("token").and_then(|t| t.as_str()) {
                // §2.4 reserved keys on a close intent: same warn-and-drop
                // rule as entries above.
                for key in reserved_intent_keys(e) {
                    tracing::warn!(
                        target: "strategy",
                        strategy = %strategy_name,
                        token = %token,
                        key,
                        "reserved key dropped (stop-loss / take-profit / max-hold sizing belongs to the kernel)"
                    );
                }
                let reason = e
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("strategy")
                    .to_string();
                // #935 fix 2: the advisory is opt-IN — an absent/absent-minded
                // `use_maker` stays the historic taker sweep (advisory
                // default-false; the kernel gate does the rest).
                let use_maker = e
                    .get("use_maker")
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false);
                // #915: the optional maker-TP limit rides the same opt-in
                // shape — an absent/malformed key stays `None` (the historic
                // price-less row).
                let exit_price = e.get("price").and_then(|p| p.as_str()).map(str::to_string);
                exit_intents.push(StrategyExitIntent {
                    token_id: token.to_string(),
                    reason,
                    // The intent closes THIS scope's row (the emitting scope's
                    // account — the same second slice key the held view uses).
                    account_id: scope.to_string(),
                    use_maker,
                    exit_price,
                });
            }
        }
    }
    if let Some(serde_json::Value::Array(br)) = v.get("breaks") {
        parse_breaks(&serde_json::Value::Array(br.clone()))
            .into_iter()
            .for_each(|b| breaks_out.push(b));
    }

    let mut candidates = Vec::new();
    let Some(entries) = v.get("entries").and_then(|e| e.as_array()) else {
        return candidates;
    };
    for e in entries {
        let (Some(token), Some(price_s)) = (
            e.get("token").and_then(|t| t.as_str()),
            e.get("price").and_then(|p| p.as_str()),
        ) else {
            continue;
        };
        let Ok(price) = Decimal::from_str(price_s) else {
            continue;
        };
        // §2.4 reserved keys (API 1.0): stop-loss / take-profit / max-hold
        // sizing belongs to the kernel (exit policy / risk), never to a
        // strategy intent. A reserved key is logged under target
        // `strategy` and DROPPED — the entry itself is never refused (a
        // stray field must not cost a signal) and `TradeSignal` has no
        // field it could reach, so the entry result is exactly what this
        // intent would have produced without it.
        for key in reserved_intent_keys(e) {
            tracing::warn!(
                target: "strategy",
                strategy = %strategy_name,
                token = %token,
                key,
                "reserved key dropped (stop-loss / take-profit / max-hold sizing belongs to the kernel)"
            );
        }
        // OPTIONAL per-entry share count (decimal STRING, the ABI's numeric
        // form). Absent = the kernel sizes from its notional budget; present
        // but malformed = ignore it and fall back to kernel sizing, never
        // reject the entry (a typo must not cost the signal).
        let shares = e
            .get("shares")
            .and_then(|s| s.as_str())
            .and_then(|s| Decimal::from_str(s).ok());
        // #642 (RFC 568): the per-entry `pair` field — the Lua mirror.
        // Per-entry wins over the library's load-time default; both absent
        // (or the per-entry one malformed) = None = old behaviour bit-for-bit.
        // Labels are readable strings carried verbatim.
        let pair_decl = e
            .get("pair")
            .and_then(|p| p.as_object())
            .and_then(|p| {
                let key = p.get("key").and_then(|k| k.as_str())?;
                let leg = p.get("leg").and_then(|l| l.as_str())?;
                Some(crate::signal::TradePairDecl {
                    key: key.to_string(),
                    leg: leg.to_string(),
                })
            })
            .or_else(|| pair_default.cloned());
        let reason = e
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("strategy entry")
            .to_string();
        // Resolve asset/condition/direction from the round's markets. A
        // token outside this round cannot be traded and is dropped.
        for m in ctx.markets() {
            if m.up_token_id == token {
                candidates.push(TradeSignal {
                    strategy: strategy_name.to_string(),
                    account_id: scope.to_string(),
                    asset: m.asset.clone(),
                    direction: crate::model::SignalDirection::Up,
                    token_id: m.up_token_id.clone(),
                    condition_id: m.condition_id.clone(),
                    price,
                    reason: reason.clone(),
                    shares,
                    pair: pair_decl.clone(),
                });
                break;
            }
            if m.down_token_id == token {
                candidates.push(TradeSignal {
                    strategy: strategy_name.to_string(),
                    account_id: scope.to_string(),
                    asset: m.asset.clone(),
                    direction: crate::model::SignalDirection::Down,
                    token_id: m.down_token_id.clone(),
                    condition_id: m.condition_id.clone(),
                    price,
                    reason: reason.clone(),
                    shares,
                    pair: pair_decl.clone(),
                });
                break;
            }
        }
    }
    candidates
}
