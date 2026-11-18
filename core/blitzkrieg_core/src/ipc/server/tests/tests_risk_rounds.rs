//! `tests_risk_rounds` split into domain submodules (pure code motion).
//! Each submodule nails one wire contract; fixtures and helpers live in the
//! parent `ipc::server::tests` module and are reached via `super::super::*`.

mod risk_db_failclosed;
mod risk_exit;
mod risk_persist;
mod risk_readout;
mod risk_systemic;
mod round_duration;
mod soft_unload;
mod system_reset;
