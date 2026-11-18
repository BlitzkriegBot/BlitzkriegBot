//! The fee-model contract (#1279) — **contract stage only**.
//!
//! The kernel charges one hardcoded curve (`core/blitzkrieg_core/src/exit_fees.rs`
//! `FeeSchedule`), which means a new exchange cannot state its own fee schedule
//! without editing the kernel — the exact thing `DEV.md` §2.1 ("内核零市场代码")
//! and §9.2 ("新交易所 = 一个扩展 crate + Cargo feature，内核不改一行") forbid.
//! This module is the missing seam: the venue declares its curve, the kernel
//! asks for it.
//!
//! **Nothing here is wired.** No implementation, no caller, no kernel change.
//! Issue #1279 requires the interface to be approved by a human operator before
//! any implementation work starts, so this file deliberately carries the shape
//! and the reasoning and nothing else. The migration note that goes with it is
//! `docs/FEE_MODEL_CONTRACT_MIGRATION.md`.
//!
//! Why `bk_contract` and not `blitzkrieg_core`: every `extensions/*/Cargo.toml`
//! depends on `blitzkrieg-bk-contract` and on nothing else internal (#841 — an
//! edge to the kernel would form a cycle, and `DEV.md` §2.1 rule 2 forbids it).
//! A trait defined anywhere else would be invisible to the very crates that have
//! to implement it.
//!
//! Why the return unit is USD PER SHARE and not a percentage of price: the
//! venue publishes per-share ("`fee = C x feeRate x p x (1-p)`"), and
//! `exit_fees.rs::FeeSchedule::fee_per_share` is documented as "THE one spelling
//! of the curve in the kernel", with the percentage form derived from it. A
//! contract that asked for the derived unit would put a second arithmetic path
//! (a division) in front of the only number the ledger books.

use rust_decimal::Decimal;

/// Which side of the book a fill sat on.
///
/// A typed role rather than the `is_maker: bool` the sketch in #1279 carries:
/// the two are not symmetric here, and the asymmetry is money. The kernel
/// charges makers nothing and *credits* them a modeled rebate
/// (`service/settlement_orders/orders.rs`, `model_maker_rebate`), i.e. a maker
/// fill moves cash the opposite way — a `bool` at a contract boundary is exactly
/// where that sign gets lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeeRole {
    /// Rested on the book and was taken.
    Maker,
    /// Crossed the spread and took.
    Taker,
}

impl FeeRole {
    pub fn is_maker(self) -> bool {
        matches!(self, Self::Maker)
    }
}

/// A venue's own fee curve: what it charges per share, in USD, at `price`.
///
/// #1279 contract stage — **no caller and no implementation yet**; the shape is
/// submitted for operator approval before any kernel or extension is touched.
///
/// Implementors answer in **USD per share** (see the module note). A negative
/// return is a REBATE — cash the venue pays the account — and the caller must
/// book it on the credit side, not as a negative charge. Today's kernel keeps
/// rebates on a separate cashflow channel and charges the taker curve only; a
/// `FeeModel` that returns negative numbers therefore cannot be wired under the
/// current booking rules without a decision on that channel.
///
/// The two methods are separate because a venue may price the two directions
/// differently (exit-only fees, entry rebates, tiered round trips). Today's
/// shipped curve does not: `exit_fees.rs::taker_fee_pct` is one function called
/// from both the entry and the exit charge site, so a Polymarket implementation
/// returns the same number from both — that equality is the implementation's
/// obligation to state, not the contract's to assume.
///
/// On `#[allow(dead_code)]`: measured, not assumed. This is `pub` API of a `pub
/// mod` in an rlib, which the `dead_code` lint never fires on — `cargo clippy -p
/// blitzkrieg-bk-contract --all-targets -- -D warnings` is clean with and without
/// the attribute. It is therefore left OFF: a no-op attribute is noise a reviewer
/// has to reason about, while the "contract stage, nothing wired yet" fact is
/// stated where it belongs, in prose. If this module is ever made private, the
/// lint will say so and the attribute can come back then.
pub trait FeeModel: Send + Sync {
    /// The declared name, for reporting and for the gates that pin it. Every
    /// other plugin-boundary trait names itself (`DataFeed::name`,
    /// `MarketPlugin::name`, …), so this one does too.
    fn name(&self) -> &str;

    /// The fee charged when the position is OPENED, per share, in USD.
    fn entry_fee(&self, price: Decimal, role: FeeRole) -> Decimal;

    /// The fee charged when the position is CLOSED, per share, in USD.
    fn exit_fee(&self, price: Decimal, role: FeeRole) -> Decimal;
}
