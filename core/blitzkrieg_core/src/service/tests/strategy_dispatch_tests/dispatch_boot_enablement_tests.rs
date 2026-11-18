//! `dispatch_boot_enablement_tests` split into domain submodules (pure code motion).
//! Each submodule pins one boot / enablement contract; `helpers.rs` holds the
//! shared `stored_strategy_set`; everything is reached via `use super::*` from
//! this hub, which itself imports the parent `strategy_dispatch_tests` module.

mod active_pointer;
mod boot_assignment;
mod display_account;
mod handshake;
mod helpers;
mod minting;
mod scoped_enablement;
mod shadow_evolution_cell;
mod startup_selection;
mod state_file;
mod strategies_empty;
mod toggles;
