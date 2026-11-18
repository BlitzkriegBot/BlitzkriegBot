//! `dispatch_cap_gate_tests` split into domain submodules (pure code motion).
//! Each submodule pins one contract — per-strategy caps, virtual accounts,
//! cross_venue_arb through the real Lua package, arb totals, #1164 warn
//! rate-limiting, orphan sweep, and the undeclared-strategy bypass guard.
//! Shared fixtures (`core_with_engine`, `feed_entry_setup`, `strategy_entry`,
//! `dec_of`, `TargetDip`, …) live in the parent `strategy_dispatch_tests`
//! module and are reached by the children via `use super::super::*`.

mod arb_totals;
mod cap_gate;
mod cross_venue_arb;
mod helpers;
mod orphan_sweep;
mod undeclared;
mod virtual_accounts;
mod warn_tests;
