//! v0.4.3 architecture contracts — **frozen definitions only** (Gitea #190, T0).
//!
//! This module contains NOTHING but `trait` / `struct` / `enum` declarations and
//! their documentation. No call site is wired, no existing type is modified, no
//! behaviour changes. It exists so the seven contract groups below can be
//! approved (or rejected) as *text* before any implementation touches the money
//! path — the operator ruling this follows is 「契约先行：凡涉及核心接口的变更，
//! 必须先提交 Trait/Struct 定义，经运营者审批后再写实现」.
//!
//! **T0 只冻结契约，接线见后续 T1–T11.** Every item here is therefore
//! `#[allow(dead_code)]` at module scope: an unused declaration is the expected
//! state of a frozen contract, not a defect. Consumers:
//!
//! | Contract group | Consumed by | Audit id |
//! |---|---|---|
//! | `readonly` — `MethodSpec` / `MethodKind` / `ReadonlyPolicy` | T3 | P0-3 |
//! | `store` — `StoreRead` / `StoreWrite` / `WriteOutcome` | T4 | P0-6 |
//! | `metrics` — `Tally` / `AccountMetrics` / `METRICS_SNAPSHOT` | T6 | P1-1 / P0-5 |
//! | `price` — `PriceState` / `EstimateBasis` | T2 (with P0-2 wiring) | P0-2 |
//! | `halt` — `HaltScope` / `Halt` | T10 | P1-7 |
//! | `order` — `OrderState` / `TerminalEvidence` / `PageRequest` / `Page` | T7, T9 | P1-4 / P1-6 |
//! | `attribution` — `VenueTrade.account` / `FoldVerdict` | T2 | P0-1 |
//!
//! Source of truth for every claim in this module: the v0.4.3 architecture audit
//! `dev/audit/audit-architecture-v043-2026-11-24.md` (Step 1 defect sections,
//! Step 2 task table) checked against the code at `gitea/main = 5b88cecf`. The
//! `file:line` anchors in the docs below were read at that commit, not recalled.
//!
//! Consequences of the freeze (stated once, applies to every item):
//!
//! - **These definitions win over the old shapes only after T1–T11 are approved.**
//!   Until then the existing types keep running unchanged; this module is inert.
//! - Changing anything here after approval requires **re-approval** (issue #190
//!   「契约冻结后如需修改，须重新审批」) — do not "fix" a frozen signature
//!   mid-implementation.
//! - Several names collide with existing items (`StoreApi`, `WriteOutcome`,
//!   `MemoryOnlyReason`, `AccountScope`, `PriceSource`, `VenueTrade`). That is
//!   deliberate: the contracts keep their agreed spelling, and the *old* item is
//!   what the consuming task deletes. Nothing here shadows the old item — the
//!   paths differ (`crate::store::…` vs `crate::contracts::…`) — so both coexist
//!   while the wiring lands.
//!
#![allow(dead_code)] // T0 只冻结契约：未接线即未使用，接线见后续 T1–T11

pub mod attribution;
pub mod halt;
pub mod ids;
pub mod metrics;
pub mod order;
pub mod price;
pub mod readonly;
pub mod store;

pub use attribution::{FoldVerdict, UnattributedFill, VenueTrade, VenueTradeFacts};
pub use halt::{Halt, HaltScope};
pub use ids::{PositionId, VenueId, VenueOrderId};
pub use metrics::{AccountMetrics, METRICS_SNAPSHOT, Tally};
pub use order::{
    OrderState, Page, PageRequest, ProbeResult, TerminalEvidence, TerminalKind, VenueAck,
};
pub use price::{EstimateBasis, PriceSource, PriceState};
pub use readonly::{MethodKind, MethodSpec, ReadonlyPolicy};
pub use store::{
    AccountRecord, LedgerProjection, MemoryOnlyReason, SettingKey, SettingValue, StoreApi,
    StoreError, StoreRead, StoreTx, StoreWrite, StrategySet, WriteOutcome,
};
