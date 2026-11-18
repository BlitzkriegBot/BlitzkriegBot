//! Settlement timing, retry and price-source constants.
//! Moved verbatim from `settlement.rs` (pure code motion).

use crate::model::ExitReason;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// First retry delay after a failed redemption.
pub const RETRY_BASE_MS: i64 = 5_000;
/// Ceiling on the exponential retry delay: a redemption that keeps failing is
/// retried, but never as a storm.
pub const RETRY_MAX_MS: i64 = 300_000;
/// How long an unanswered settlement query waits before the core asks again.
pub const REQUERY_MS: i64 = 60_000;
/// Unanswered queries past this age mean the venue half is not running at all
/// (no credentials, or the extension died) — reported as a blind settlement
/// path rather than silence.
pub const BLIND_AFTER_MS: i64 = 300_000;
/// How much later than its market's end a position may have been opened and
/// still be treated as that market's position (a venue fill is reported a little
/// after the round it belongs to).
///
/// This is the sanity check that keeps a settlement from being *invented*.
/// `expires_at_ms` is the venue's own round end on the engine path, but the
/// order path derives it from the order's declared `round_slot`
/// (`(slot + 1) * round_duration_sec`, see `Core::apply_delta_effects`), and an
/// order that declares a slot on some other clock — the parity fixtures declare
/// `roundSlot: 1` against an epoch clock, which lands in 1970 — carries a value
/// that is not a market end at all. Settling on it would book a real receivable
/// against a market that is still running, so such a position is never watched
/// for settlement: it stays open in the book where an operator can see it,
/// instead of being closed at a made-up payout.
pub const EXPIRY_TRUST_MS: i64 = 300_000;
/// #684: how long a watched market may stay UNANSWERED past its first query
/// before the expiry fallback sweep stops waiting. The venue's round list only
/// carries rounds ending in the next ~20h, so an ALREADY-EXPIRED condition can
/// never appear in a settlement answer — the query re-arms every
/// [`REQUERY_MS`] forever and the position is a ghost until an operator
/// notices. That silence is the observable signature: a venue that CAN answer
/// does so within one re-query interval (every answer counts, including a
/// `resolved: false` "not yet"). Unanswered past this bound means the venue
/// will never rule on this condition, structurally.
///
/// Operator ruling 2026-11-04 ("15 分钟的轮次持仓一个多小时 怎么可能"): the
/// bound was 2h and round trips drift far past their own expiry before the
/// sweep ever closed them while the venue's structural silence is already
/// legible after two re-query cycles. 120s = two requery intervals — the
/// same evidence, caught before a 15-minute round has outlived its length.
pub const EXPIRY_FALLBACK_AFTER_MS: i64 = 120_000;
/// #1236（操作者裁决 2026-11-15）：**过期轮次必须强制结算——轮次过期 10s
/// 内完成**。binary 市场到期时价格天然收敛到 1/0（实机截图：过期仓位
/// Cur = 0.99 / 0.01 / 0.97），最后报价就是裁决结果的最佳估计；过期 10s
/// 后平台侧结算已可确认，等待不再以幽灵仓位为代价。裁决晚到的真实现金
/// 仍由 reconcile sweep 对平，与 #991 的现金裁决点一致。此时长取代原
/// 120s 沉默窗（#930/#684）：强制结算不再区分 venue 是否应答。
pub const EXPIRY_ESTIMATE_SETTLE_AFTER_MS: i64 = 10_000;
/// The exit reason a settled position closes with (`exit:settlement`).
pub const SETTLEMENT_EXIT_REASON: ExitReason = ExitReason::Settlement;
/// #684: the exit reason a fallback-closed position carries
/// (`unverifiable_expiry` on the wire). Deliberately NOT
/// [`ExitReason::Settlement`]: no venue verdict was ever received, so the
/// trade record must never read as one — the close price is a bookkeeping
/// value, not a payout, and the label is what makes that auditable forever.
pub const UNVERIFIED_EXPIRY_EXIT_REASON: ExitReason = ExitReason::UnverifiableExpiry;

/// The closed-position `price_source` spelling (issue #1002) for a fallback
/// close priced at the position's LAST KNOWN MID — a real feed observation,
/// though not a venue verdict.
pub const PRICE_SOURCE_CURRENT: &str = "current";
/// The closed-position `price_source` spelling (issue #1002) for a fallback
/// close priced at the ENTRY price (dead feed). Exposed to flag self-favoring
/// "moved 0%" records.
pub const PRICE_SOURCE_ENTRY_FALLBACK: &str = "entry_fallback";

/// The closed-position `price_source` spelling (issue #1238) for a binary
/// fallback close: `> 0.5` settles `1`, `<= 0.5` settles `0`. Binary
/// settlement domain is {0, 1} — a coordinate fix, not precision; 0.5 is
/// hard-coded, never a config knob (issue #1238).
pub const PRICE_SOURCE_BINARY_THRESHOLD: &str = "binary_threshold";
/// The closed-position `price_source` spelling (issue #1238) when no price
/// was ever observed (`current_price == 0`). Entry price / last mid / mean
/// are NOT substituted: booked price is 0, notional rides the receivable
/// pool, venue truth redeems it later.
pub const PRICE_SOURCE_UNOBSERVED: &str = "unobserved";

/// Issue #1273: the single fallback price shared by the expiry sweep
/// (`run_expiry_fallback_sweep`) and the manual force close
/// (`force_close_expired_position`) — one threshold, one price-source set,
/// both code paths, so a position can never be priced at 0.55 by one path
/// and binary 1/0 by the other.
///
/// Binary settlement domain is {0, 1}:
///   * `current_price > 0.5` → 1, `price_source = binary_threshold`
///   * `0 < current_price <= 0.5` → 0, `price_source = binary_threshold`
///   * `current_price == 0` → 0, `price_source = unobserved` — the dead feed
///     is never repriced at entry price, last mid, or any mean.
pub fn fallback_binary_price(current_price: Decimal) -> (Decimal, &'static str) {
    if current_price.is_zero() {
        return (Decimal::ZERO, PRICE_SOURCE_UNOBSERVED);
    }
    (
        if current_price > dec!(0.5) {
            Decimal::ONE
        } else {
            Decimal::ZERO
        },
        PRICE_SOURCE_BINARY_THRESHOLD,
    )
}
