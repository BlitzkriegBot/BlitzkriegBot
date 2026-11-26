//! The strategy/plugin surface, moved verbatim from `service.rs` (pure code
//! motion, no logic edits): strategy loading/unloading and the Lua package
//! scanner, the enable/scope/compat handshake (E27), hot-parameter writes,
//! the shadow-evolution lifecycle, the unified cross-venue mapping ledger's
//! IPC verbs, the extension registry listing and the trade-summary reports.
//!
//! Domain submodules live in `strategies/`; this file keeps only the small
//! accessors that do not belong to a single domain.

use crate::ome::Ome;
use crate::position::PositionManager;

use super::Core;
use super::config::CoreConfig;

mod enable;
mod loading;
mod params;
mod reports;
mod shadow_evolution;
mod unified;

impl Core {
    pub fn ome(&self) -> &Ome {
        &self.ome
    }

    pub fn positions(&self) -> &PositionManager {
        &self.positions
    }

    /// Mutable access to the position manager — TEST SEAM ONLY (test cfg or
    /// feature `test-support`; #780/#825 boundary hygiene, the same gate
    /// [`Self::ledger_mut`] carries). The #364 IPC tests seed closed rows to
    /// replay. Not for moving money: every cash path routes through the
    /// ledgers, not this book — and no production caller may reach the book
    /// bluntly.
    #[cfg(any(test, feature = "test-support"))]
    pub fn positions_mut(&mut self) -> &mut PositionManager {
        &mut self.positions
    }

    /// Mutable handle on the account book — TEST SEAM ONLY (test cfg or
    /// feature `test-support`; #780/#825 boundary hygiene, the same gate
    /// [`Self::ledger_mut`] carries). The #364 IPC tests prime a ledger
    /// before replaying history. The LIVE money paths never come through
    /// here: they route by the order's own `account_id` at each site (§9.3).
    #[cfg(any(test, feature = "test-support"))]
    pub fn accounts_mut(&mut self) -> &mut crate::account::AccountLedgers {
        &mut self.accounts
    }

    /// Mutable access to config — TEST SEAM ONLY (test cfg or feature
    /// `test-support`; #780/#825 boundary hygiene). Production writers go
    /// through narrow mutators (e.g. [`Self::set_tick_schedule_ms`]) or the
    /// runtime-config paths, so a naked config write can no longer bypass
    /// their validation from outside `service`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn config_mut(&mut self) -> &mut CoreConfig {
        &mut self.config
    }

    /// Read-only handle on the closed-trade ledger — TEST SEAM ONLY (test cfg
    /// or feature `test-support`, the same gate [`Self::positions_mut`] carries).
    ///
    /// A test that has to prove something about the LEDGER (a page window, a
    /// per-account split, an id baseline) must seed rows through the real
    /// carrier. Production never reads this field from outside `service`; the
    /// writes there all go through the close/redemption paths so they carry
    /// their own receipts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn trade_log(&self) -> Option<&crate::db::trades::TradeLog> {
        self.trade_log.as_ref()
    }

    /// Point the exit ladder's deadline grid at the step the caller will
    /// actually drive (#390 replay nofast). `tick_schedule_ms` is the grid
    /// the counter ratchets backfill over (consumed by
    /// `check_exits_interval`), so a non-positive step would corrupt the
    /// schedule — normalized to 1ms, the same floor the replay driver used
    /// to apply at its call site before the naked `config_mut` write was
    /// closed (#780).
    pub fn set_tick_schedule_ms(&mut self, tick_ms: i64) {
        self.config.tick_schedule_ms = tick_ms.max(1);
    }

    /// Read-only view of the active config (diagnostics/backtest reports).
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }
}
