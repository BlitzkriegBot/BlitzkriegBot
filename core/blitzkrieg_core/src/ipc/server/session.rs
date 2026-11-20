//! Accept loop and per-connection session wiring (issue #780: split out of
//! the parent module; pure move).

use super::*;

/// Run the UDS server until the shutdown oneshot fires or a signal arrives.
///
/// #841: the market plugins are linked and registered ONLY by the composition
/// root (core/blitzkrieg_launcher) — this server RECEIVES the registry instead
/// of compiling them in. The caller's registry is the whole compiled-in plugin
/// face of this boot: boot enablement, multi-plugin data planes and the IPC
/// readouts all resolve against it.
pub async fn run(
    socket_path: String,
    mut config: CoreConfig,
    tick_ms: u64,
    shutdown: oneshot::Receiver<()>,
    registry: crate::market::registry::MarketPluginRegistry,
) -> anyhow::Result<()> {
    // 任务二 1-3: 启动时间线 — 从 server::run 入口到 IPC 监听就绪，用于定位
    // 面板首屏空白是否在内核 boot 阶段。
    let boot_start = now_ms();
    eprintln!("blitzkrieg-core: boot start t={boot_start}");
    // Event plumbing: Core -> mpsc -> broadcast -> every session.
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<Event>();
    let (bus_tx, _) = broadcast::channel::<Event>(512);
    {
        let bus_tx = bus_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = evt_rx.recv().await {
                let _ = bus_tx.send(ev);
            }
        });
    }

    // 进化引擎的两个开关：`kernel_meta` 是家（`.env` 的 `SHADOW_*` 只是旧部署
    // 的回退）。必须在 Core 构造前读 —— 它们决定引擎启动时是否 scaffolding。
    //
    // issue #46（fail-closed）：`AccountRiskDb` 打开失败或 `kernel_meta` 读取
    // 失败不得静默回退 `.env` 拼写——那样会让内核在 operator 不察觉的
    // 情况下以旧键启动，而与之对齐的主 boot 路径（下方的
    // `open_account_risk_db`）是拒绝启动的。打开/读取失败一律 `?` 向上
    // 抛结构化错误；只有主键缺失（`Ok(None)`）才不是失败，那时再沿用
    // env 拼写。
    let meta_db_path = crate::db::default_db_path();
    let meta_db = crate::risk::account_store::AccountRiskDb::open(&meta_db_path).map_err(|e| {
        anyhow::anyhow!(
            "account risk DB open failed at {} ({e}) — refusing to boot \
                 (fail-closed: kernel metadata store cannot be read; the env spelling \
                 is not a fallback — issue #46)",
            meta_db_path.display()
        )
    })?;
    let raw = meta_db
        .load_kernel_meta(crate::risk::account_store::SHADOW_ENABLED_META)
        .map_err(|e| {
            anyhow::anyhow!(
                "kernel meta `shadow_enabled` read failed ({e}) — refusing to boot \
                 (fail-closed: env fallback is retired — issue #46)"
            )
        })?;
    if let Some(raw) = raw {
        match raw.trim() {
            "true" => config.shadow_evolution_enabled = true,
            "false" => config.shadow_evolution_enabled = false,
            other => eprintln!(
                "blitzkrieg-core: WARNING: kernel_meta shadow_enabled=`{other}` is not \
                 true/false — the env spelling stands"
            ),
        }
    }
    let raw = meta_db
        .load_kernel_meta(crate::risk::account_store::SHADOW_AUTO_EVOLVE_META)
        .map_err(|e| {
            anyhow::anyhow!(
                "kernel meta `shadow_auto_evolve` read failed ({e}) — refusing to boot \
                 (fail-closed: env fallback is retired — issue #46)"
            )
        })?;
    if let Some(raw) = raw {
        match raw.trim() {
            "true" => {
                config
                    .shadow_evolution_tuning
                    .get_or_insert_with(Default::default)
                    .auto_evolve = Some(true)
            }
            "false" => {
                config
                    .shadow_evolution_tuning
                    .get_or_insert_with(Default::default)
                    .auto_evolve = Some(false)
            }
            other => eprintln!(
                "blitzkrieg-core: WARNING: kernel_meta shadow_auto_evolve=`{other}` is not \
                 true/false — the env spelling stands"
            ),
        }
    }

    // issue #1199 PR-2: the database opens BY DEFAULT. The path comes from
    // `db::default_db_path` — the SAME resolver the launcher's one-shot
    // import uses — so the file the import fills is the file the kernel
    // reads (the split brain that froze the default deployment's DB is dead:
    // the launcher imported into the default path while the session only
    // opened an explicit `BLITZKRIEG_DB_PATH`, so the panel's edits fell
    // back to .env while the DB sat unread). A failure to open is
    // FAIL-CLOSED for BOTH spellings (issue #1189 §5 posture, extended):
    // the kernel refuses to run rather than silently trading on the env
    // fallback while the operator believes the DB is authoritative. The
    // error text distinguishes the two causes — an explicit path that will
    // not open vs. the default path that cannot be created.
    //
    // issue #1199 R2: the DEFAULT spelling is install-root anchored (the exe's
    // ancestors — the #380 convention), so the same binary resolves the SAME
    // database from any cwd: a cwd-relative default used to create a NEW empty
    // database in whatever directory the operator launched from (the empty-DB
    // split brain — the panel then read/wrote the empty file while the
    // deployment's real database sat unread). An explicit relative
    // `BLITZKRIEG_DB_PATH` stays honoured verbatim (the operator's spelling is
    // their choice); only the default is anchored.
    //
    // issue #1199 第六轮（操作者裁决 2026-11-14）：这条路径在 `Core::new`
    // **之前**解析并写进 `config.db_path` —— 装配以它打开 `SqliteStore`（
    // 唯一持久化后端，`.env` 只读），打开失败在装配里 fail-closed。
    let explicit_db = std::env::var(crate::db::DB_PATH_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let db_path = crate::db::default_db_path();
    config.db_path = Some(db_path.clone());
    let mut core = Core::new(config.clone());
    // issue #1199 R2: was this database file ALREADY on disk before the open?
    // A `false` here means this boot CREATED the file (and possibly its
    // parent directory) — the fact the empty-database WARN must name, so
    // "the panel is about to read and write a brand-new empty database" is
    // never silent (the split-brain root: a cwd-relative default used to
    // create a NEW empty database in whatever directory the operator
    // launched from, while the deployment's real database sat elsewhere).
    let db_file_preexisted = db_path.exists();
    let db_parent_preexisted = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.is_dir())
        .unwrap_or(true);
    if let Err(e) = core.open_account_risk_db(db_path.clone()) {
        return Err(anyhow::anyhow!(match explicit_db {
            Some(_) => format!(
                "account risk DB open failed at {e} — refusing to boot (fail-closed: \
                 fix {env} or point it at a writable file; the env fallback is retired — \
                 issue #1199)",
                env = crate::db::DB_PATH_ENV
            ),
            None => format!(
                "account risk DB open failed at the default path {} ({e}) — refusing to \
                 boot (fail-closed: the default database could not be created or opened — \
                 fix the path or permissions; the env fallback is retired — issue #1199)",
                db_path.display()
            ),
        }));
    }
    // 操作者裁决 2026-11-14（#1199 第六轮）：UI 提交的凭证住在加密的
    // `kernel_meta.credential.*` 行里；执行器在 boot 时一次性读进程环境，
    // 所以这里把行注入进程环境（仅内存——绝不写任何文件）。SQL 行压过
    // env 拼写；主键缺失/损坏时 env 拼写原样保留并大声报警。
    if let Some(db) = core.account_risk_db_handle() {
        crate::db::credentials::inject_credentials_into_process_env(&db);
    }
    // issue #1199 R2: the empty-database WARN. The open above now ALWAYS runs
    // (the default path resolves against the install root, so it is the same
    // file from any cwd); when the file carries no data at all — zero
    // `account_risk` rows AND no `import_v1` marker — the operator must hear
    // that the panel's reads and writes are about to land in an EMPTY
    // database. A database that already holds data stays silent (this is a
    // boot-shape fact, not a per-boot alarm).
    if !db_file_preexisted || core.account_risk_db_is_empty() {
        let created = if !db_file_preexisted {
            if db_parent_preexisted {
                format!("created {}", db_path.display())
            } else {
                format!(
                    "created {} (and its parent directory — the install-root default \
                     resolved to a directory that did not exist yet)",
                    db_path.display()
                )
            }
        } else {
            format!("opened {}", db_path.display())
        };
        eprintln!(
            "blitzkrieg-core: WARNING 风控库为空 — {created}; 面板读写将落此库 \
             (account_risk 零行、无 import_v1 导入标记)；旧 env 数据请用导入器 \
             (db_import, import_v1)"
        );
    }
    eprintln!(
        "blitzkrieg-core: core constructed t={} dt={} ms",
        now_ms(),
        now_ms().saturating_sub(boot_start)
    );
    core.set_event_sink(evt_tx);
    // Crash recovery FIRST: rebuild tracked orders from the durable log so any
    // order still resting at the venue is known (and thus cancellable) rather
    // than becoming an orphan. Done before the engine/feeds start.
    let recovered = core.restore_orders();
    if recovered > 0 {
        eprintln!("blitzkrieg-core: restored {recovered} live order(s) from the order log");
    }
    // Then rebuild the OPEN position book, so a restart keeps valuing and
    // exit-managing positions that were already filled before the restart.
    let recovered_pos = core.restore_positions();
    if recovered_pos > 0 {
        eprintln!(
            "blitzkrieg-core: restored {recovered_pos} open position(s) from the position log"
        );
    }
    // Seed the simulated principal in every mode that settles locally.
    //
    // This site is why `Mode` is an enum and not a `readonly: bool`: as an `==`
    // comparison against `Dry` it was invisible to the compiler, so ReadOnly
    // silently started with a zero balance, reported a seed it did not have
    // (`BalanceResult::seed` was updated separately), and rejected every entry
    // with INSUFFICIENT_FUNDS. Behaviourally that made read-only look like a
    // broken dry mode rather than a promise — the local-settlement guarantee was
    // in place, the ledger it settles into was not.
    //
    // SKIPPED when a durable cash journal replayed inside `Core::new`
    // (`config.ledger_journal_dir` set and at least one journal file on
    // disk): the boot balance is the replayed trail then, and re-seeding
    // here would overwrite it — the exact fault the journal exists to fix
    // (a restart must not vaporize the盈亏 history).
    if config.mode.settles_locally() && !core.boot_replayed_journal() {
        core.set_balance(config.dry_seed_balance);
    }

    // Market plugins: the registry arrives from the composition root
    // (core/blitzkrieg_launcher, #841) — the core drives its components
    // through the market seam without naming any venue.
    // E27 (§8.2): registration at the composition root MUST happen before
    // `install_engine` runs here — it loads and enables strategies, and the
    // startup `--enable-strategy` request and both IPC handshake sites judge
    // against the same declaration snapshot. `None` would leave the handshake
    // inert.
    // The enablement chain resolves here — four layers, first non-empty wins:
    // 1. an explicit `--market-plugin` flag — everything else is ignored
    //    (replays and the corpus baselines pin their plugin this way);
    // 2. 绑定即启用: the account book's venue bindings — adding a venue's API
    //    account IS enabling that venue's plugin; ANY derived set outranks the
    //    persisted cell, which the boot then retires (migration, one INFO
    //    line, inside `boot_enablement`);
    // 3. the persisted cell (the old market.plugin.set write) — kept alive
    //    only while NO binding speaks;
    // 4. first-registered — inside `active_market_plugins`, the historical
    //    fallback.
    let mut plugin = config.market_plugin.clone();
    if plugin.is_none() {
        // Multi-plugin data plane: the value is a comma-separated list
        // ("polymarket,kalshi"; a single name is the historical form). A cell
        // naming ANY plugin that is not compiled in is refused as a whole
        // (same posture as the single-value cell — a stale entry must not
        // silently boot half a data plane), falling back to the next layer.
        let cell = config
            .runtime_config_path
            .as_deref()
            .and_then(|p| crate::runtime_config::load_market_plugin_from(std::path::Path::new(p)))
            .filter(|cell| {
                let names = crate::market::parse_plugin_list(&cell.plugin);
                !names.is_empty() && names.iter().all(|n| registry.get(n).is_some())
            });
        if let Some(cell) = &cell {
            eprintln!(
                "blitzkrieg-core: market plugin '{}' (persisted UI setting, \
                 data/runtime/config.json; an explicit --market-plugin and the \
                 accounts' venue bindings outrank it)",
                cell.plugin
            );
        } else {
            // A cell that loaded but did not arm (all-commas junk or an
            // unregistered name) keeps its historical refusal warning — the
            // operator wrote SOMETHING and must hear why it did not apply.
            if let Some(raw) = config
                .runtime_config_path
                .as_deref()
                .and_then(|p| {
                    crate::runtime_config::load_market_plugin_from(std::path::Path::new(p))
                })
                .map(|c| c.plugin)
                .filter(|v| !crate::market::parse_plugin_list(v).is_empty())
            {
                eprintln!(
                    "blitzkrieg-core: persisted market plugin '{}' is not fully compiled in — \
                     the cell is ignored (falling back to the flag/bindings/first-registered)",
                    raw
                );
            }
        }
        // The chain resolves through the shared `boot_enablement` — the same
        // function the readout re-derives with, so `market.plugin.get` can
        // never disagree with this boot: it returns the cell value (layer 3),
        // the DERIVED BINDING SET (layer 2 — the spec every venue's data
        // plane mounts from), or lets the fallback resolution run (layer 4).
        let (resolved, source) =
            crate::market::boot_enablement(&core, &registry, None, cell.as_ref());
        plugin = resolved;
        if source == crate::market::PluginEnableSource::Accounts {
            let venues = crate::market::derived_venue_set(&core);
            eprintln!(
                "blitzkrieg-core: market plugins {venues:?} enabled from the accounts' \
                 venue bindings (绑定即启用) — market.plugin.set is refused while this \
                 derivation is live"
            );
        }
    }
    let actives = crate::market::active_market_plugins(&registry, plugin.as_deref());
    // Strategy handshake, multi-plugin: EVERY selected plugin's declaration is
    // snapshotted and a strategy loads when ANY ONE active plugin can serve
    // it — a venue the strategy cannot serve must not block the venues it
    // can. An empty list (backtester, market-free builds) leaves the
    // handshake inert.
    core.set_plugin_modes(
        actives
            .iter()
            .map(|p| crate::compat::PluginModes::for_plugin(p.as_ref()))
            .collect(),
    );

    if config.engine_enabled {
        // A refusal here (the #265 startup self-check: an explicit
        // `--enable-strategy` that resolved to nothing) ends the boot BEFORE
        // the socket is bound or a feed is started — the failure mode is a
        // kernel that looks healthy and trades none of what it was told to run.
        if let Err(why) = config.install_engine(&mut core) {
            anyhow::bail!("{why}");
        }
        // The accounts source's strategy assignments, applied at boot: every
        // account with a non-empty strategies key gets those scopes
        // instantiated AND enabled (assignment IS enablement). The one-time
        // migration materializes the default account's implicit enabled set
        // into the SQLite `account_settings.strategies` row
        // (`accounts/persist.rs:966-984`), after which that row is the only truth
        // (empty = the account runs nothing). Production-only: the replay/
        // backtest boot goes through `install_engine` alone and its
        // single-default book stays byte-identical (the exit-economics
        // baseline).
        let migrated = core
            .apply_account_strategy_assignments_at_boot()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if !migrated.is_empty() {
            eprintln!(
                "blitzkrieg-core: strategy assignment migration — the default account now \
                 explicitly runs {:?} (account_settings.strategies row; empty assignments mean a \
                 strategy-less account, there is no inherit)",
                migrated
            );
        }
        eprintln!("blitzkrieg-core: self-driving engine enabled");
    }
    let core = Arc::new(AsyncMutex::new(core));

    // The env-file watcher (operator ruling 2026-11-08): an EXTERNAL hand
    // edit of the cockpit families in `.env` re-derives the working copies
    // within one poll — a hand edit is a first-class write, never a silent
    // no-op. 2s mtime poll, dependency-free; the derived changes carry the
    // audit line so the operator sees what their edit did. (The re-derive
    // itself is the env-carrier backend's freshness mechanism — see the
    // carrier asymmetry note below.)
    //
    // issue #1199 (the watcher asymmetry): with the SQLite store open the DB
    // is the cockpit carrier, so the cockpit re-derive is DISABLED — any
    // unrelated `.env` write (an account toggle, a credential save — they all
    // rewrite the same file) used to re-derive the cockpit working copies
    // FROM THE FILE and clobber the DB-sourced values with stale keys. The
    // execution-policy reload stays: its carrier is still the env file, and
    // the reload is a pure re-read (no clobber of DB-sourced state).
    if let Some(watch_path) = core
        .lock()
        .await
        .cockpit_env_path()
        .map(std::path::Path::to_path_buf)
    {
        let core = core.clone();
        tokio::spawn(async move {
            let mut last = std::fs::metadata(&watch_path)
                .and_then(|m| m.modified())
                .ok();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let cur = std::fs::metadata(&watch_path)
                    .and_then(|m| m.modified())
                    .ok();
                if cur.is_some() && cur != last {
                    last = cur;
                    let mut c = core.lock().await;
                    // The cockpit re-derive is the env-carrier backend's
                    // freshness mechanism only (see the note above).
                    if c.cockpit_env_rederive_allowed() {
                        match c.refresh_cockpit_from_env_file() {
                            Ok(applied) if !applied.is_empty() => {
                                tracing::info!(
                                    target: "risk",
                                    "cockpit config re-derived from the env file \
                                    (external edit): {}",
                                    applied.join("; ")
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("cockpit env-file watch: re-derive failed: {e}")
                            }
                        }
                    }
                    // Same external edit: execution_policy's POLICY_* keys are
                    // resolved SQL-first (`load_for_reload_with_sql`). While a
                    // SQL row exists, a hand edit of the `.env` POLICY_* keys
                    // does NOT take effect — SQL outranks the file. This reload
                    // re-resolves through that priority so a SQL-driven change
                    // lands without a restart; the policy carrier IS SQLite
                    // (no `.env` write face), so this runs regardless of the
                    // cockpit carrier.
                    if let Err(e) = c.reload_execution_policy() {
                        tracing::warn!(
                            "execution_policy env-file watch: reload failed ({e}); \
                             the in-memory policy stands until the next write or restart"
                        );
                    }
                }
            }
        });
    }

    // 任务二 1-3: the NTP clock-offset probe. PRODUCTION-ONLY (the same
    // structural posture as `apply_account_strategy_assignments_at_boot`):
    // this block lives in `server::run`, which the replay/backtest driver
    // NEVER enters — a replay touches no NTP socket. The empty host (the
    // documented `--ntp-host ""` spelling) spawns ZERO tasks via
    // `should_spawn_ntp`; interval 0 likewise. The measurement loop runs on
    // its own thread (blocking UDP, 1.5 s timeout worst case) and only takes
    // the core lock for the instant it reports one result.
    {
        let ntp_host = config.ntp_host.clone();
        let ntp_interval = config.ntp_interval_secs;
        if crate::probe::should_spawn_ntp(&ntp_host) && ntp_interval > 0 {
            let core = core.clone();
            // The probe thread has no reactor: capture the Handle HERE (we are
            // inside the async runtime) and spawn through it from the closure.
            let rt = tokio::runtime::Handle::current();
            crate::probe::spawn_loop(
                ntp_host,
                ntp_interval,
                std::sync::Arc::new(move |offset| {
                    // Fire-and-forget: the probe never blocks trading on NTP.
                    let core = core.clone();
                    rt.spawn(async move {
                        let mut c = core.lock().await;
                        c.note_ntp_offset(offset, now_ms());
                    });
                }),
            );
            eprintln!("blitzkrieg-core: NTP clock-offset probe started (every {ntp_interval}s)");
        }
    }

    // The data plane's runtime handle: feeds/discovery mount THROUGH this
    // host, at boot AND later (account.venue.set re-binding an account to a
    // venue whose plugin never booted mounts that venue's components here —
    // the runtime half of 绑定即启用; see `enable_venue_at_runtime`).
    // The boot-resolved data-plane shape the venue.set write consults: the
    // mount flags and the already-mounted (venue, duration) discovery pairs
    // (idempotence guard) — the global family per boot plugin, plus every
    // account `rounds` row (轮次多族's boot instance pool).
    let account_families: Vec<crate::account::Account> = core
        .lock()
        .await
        .accounts()
        .account_list()
        .into_iter()
        .cloned()
        .collect();
    let data_plane = DataPlaneRuntime::at_boot(&config, &actives, &account_families, &registry);

    // RFC-661 M2: the feed_source reuse plane. The cell lives on the data
    // plane (`at_boot` seeded it with the boot-ACTIVE consumers' declared
    // edges — an inactive plugin registers nothing); the concrete host gets
    // an Arc-shared clone below so its `on_round_markets` fan-out reads the
    // same edges the mount path writes. `mounts` counts every REUSED-stream
    // mount attempt — the falsifiable claim「上游连接一份」reads it (one per
    // source stream, N consumers notwithstanding). A later runtime bind
    // appends its edge through `enable_venue_at_runtime` on the SAME cell.
    {
        let feed_reuse = data_plane.feed_reuse().expect("boot installs the cell");
        let declared_edges = DataPlaneRuntime::declared_feed_edges(&actives, &registry);
        if !declared_edges.is_empty() {
            let edges = feed_reuse.edges.read().expect("feed reuse edges").clone();
            eprintln!(
                "blitzkrieg-core: feed_source reuse active — upstream stream(s) of {:?} \
                 also feed(s) {:?}",
                edges
                    .iter()
                    .map(|e| e.source.as_str())
                    .collect::<std::collections::BTreeSet<_>>(),
                edges
                    .iter()
                    .map(|e| e.consumer.as_str())
                    .collect::<Vec<_>>(),
            );
        }
    }

    // B2 (轮次多族 软卸): the discovery poll-output gate, installed on the
    // CONCRETE CoreHost before it erases into the trait object. Every
    // extension discovery funnels through `CoreHost::on_round_markets`; from
    // here on, a batch whose (venue, duration) family no account declares
    // (and the global family does not cover) is dropped at the host — the
    // soft-unloaded instance keeps polling, its output never reaches the
    // engine. The closure carries the data plane's gauge + active-family
    // computation and the Core handle.
    let concrete_host = {
        let h = crate::market::host::CoreHost::new(core.clone());
        // Untagged round lists (legacy corpora / Node bridge) route their
        // orderbook subscription to the active plugin — the historical
        // single-plugin behaviour, resolved at assembly instead of hardcoded.
        h.set_active_plugin(actives[0].name());
        {
            let gate_core = core.clone();
            let gate_plane = data_plane.clone();
            h.set_poll_admission(std::sync::Arc::new(move |markets: Vec<_>| {
                let core = gate_core.clone();
                // The gate runs on the plugin's poll task; the admission
                // check is async (it takes the Core lock). The forward the
                // gate precedes blocks on that same lock anyway, so blocking
                // here changes no deadlock surface: the runtime is
                // multi-thread (`rt-multi-thread` in Cargo.toml), so
                // `block_in_place` moves the poll task off the shared
                // workers while it waits. A runtime-less caller (none
                // today) admits by default — the historical shape.
                match tokio::runtime::Handle::try_current() {
                    Ok(rt) => tokio::task::block_in_place(|| {
                        rt.block_on(gate_plane.admit_poll_batch(&core, &markets))
                    }),
                    Err(_) => true,
                }
            }));
        }
        h
    };
    // The trait-erased handle every feed/discovery/session share. It carries
    // no own-plugin identity ("" = legacy all-take on the scoped settlement
    // drains) but re-sees the SHARED active-plugin cell set above — the
    // untagged-order owner — through the Arc; the executor loop below
    // additionally names each handle with `for_plugin`.
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> = Arc::new(concrete_host.clone());

    // RFC-661 M2: the fan-out rides the CONCRETE host (every clone shares the
    // cell — the same Arc shape as poll_admission), so the upstream feed's
    // pushes and every later runtime mount re-see the same edges. The cell is
    // the DATA PLANE's (the runtime mount path writes through the same
    // Arc-shared edges; this clone makes the host's fan-out read them).
    concrete_host.set_feed_reuse(
        data_plane
            .feed_reuse()
            .expect("boot installs the cell")
            .clone(),
    );

    // P4: Rust-native feeds. Start Binance spot now (symbols known at boot); the
    // orderbook subscriptions are added when `engine.markets` arrives (from Node)
    // or from Rust-native round discovery below.
    //
    // Multi-plugin data plane: EVERY selected plugin's feed starts (each on the
    // shared CoreHost — the Core lock just queues the pushes). Tokens stay
    // per-plugin: each feed only ever receives the tokens its own venue reports.
    //
    // RFC-661 M2 (feed_source 复用管道): a plugin that declares `feed_source`
    // owns NO upstream of its own — its feed/discovery never start (the host's
    // fan-out re-registers the source's batches under its venue identity). The
    // SOURCE's own feed start bumps the reuse mount counter: the falsifiable
    // claim「上游连接一份」reads this counter (one mount per upstream stream,
    // no matter how many consumers enable). A consumer-only enablement (source
    // inactive at boot) mounts the source's DATA plane through the same loop
    // — the source's executor stays out of `actives`, so execution never
    // starts (数据消费与执行启用解耦).
    // RFC-661 M2: the source venues this boot's actives declare as their feed
    // upstream (compiled-in only) — the streams the reuse pipeline must mount
    // EXACTLY once across both the feed and the discovery walks below. Read
    // once here so the two walks agree on what "reused" means.
    let reused_sources: std::collections::BTreeSet<String> = actives
        .iter()
        .filter_map(|p| p.feed_source())
        .filter(|src| registry.get(src).is_some())
        .map(str::to_string)
        .collect();
    if config.feed_ws_enabled {
        let cfg = blitzkrieg_market_api::DataFeedConfig {
            spot_assets: config.binance_assets.clone(),
        };
        // RFC-661 M2: a consumer's own feed NEVER starts (its upstream is the
        // source's stream — a second feed would be the second connection the
        // reuse pipeline exists to prevent). The mounts this boot owes: every
        // active plugin's own feed (consumers skipped), PLUS the source's feed
        // when NO consumer's source is among the actives (consumer-only boot —
        // the data plane is self-sufficient, the executor is not invited).
        // The source is resolved through the DECLARED edges only — a plugin
        // whose feed_source names nothing compiled-in keeps its own feed.
        let mut feeds_to_start: Vec<std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>> =
            Vec::new();
        for p in &actives {
            if p.feed_source()
                .filter(|src| registry.get(src).is_some())
                .is_some()
            {
                continue;
            }
            feeds_to_start.push(std::sync::Arc::clone(p));
        }
        for src in reused_sources
            .iter()
            .filter(|src| !actives.iter().any(|a| a.name() == src.as_str()))
        {
            let Some(sp) = registry.get(src) else {
                continue;
            };
            eprintln!(
                "blitzkrieg-core: {src} consumers — mounting {src}'s data feed (no executor)"
            );
            feeds_to_start.push(sp);
        }
        // Mount-count discipline: every mount attempt of a REUSED stream
        // (a source some consumer declares) counts once — N consumers of the
        // same source still read 1. The consumer's own (skipped) start never
        // counts: it must not exist.
        let reuse = data_plane.feed_reuse().expect("boot installs the cell");
        for p in &feeds_to_start {
            if reused_sources.contains(p.name()) {
                reuse
                    .mounts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(feed) = p.data_feed() {
                if let Err(e) = feed.start(host.clone(), cfg.clone(), Vec::new()).await {
                    eprintln!(
                        "blitzkrieg-core: data feed start failed for {}: {e}",
                        p.name()
                    );
                } else {
                    eprintln!("blitzkrieg-core: feed started for {}", p.name());
                }
            }
        }
        eprintln!("blitzkrieg-core: rust-native feeds enabled (binance spot; poly on first round)");
    }

    // P5: Rust-native round discovery — the plugin finds its own markets and feeds
    // the engine + orderbook subscriptions, so Node is out of the data path.
    // Per-plugin loops: each discovery reports ONLY its own venue's markets and
    // subscribes only its own tokens.
    //
    // B1d (轮次多族): the loop walks the DECLARED (venue, duration) pairs —
    // the boot's global family on every plugin, plus every account `rounds`
    // row, plus every persisted per-account round-duration override (#1272) —
    // instead of one instance per plugin. Each pair starts its own discovery
    // with that duration baked into the config (the plugin picks its
    // series/slug from it); the pairs were pre-recorded in
    // `data_plane.discovery_mounted` by `at_boot`, and the set walk here is
    // the same information. A single-family deployment declares nothing but
    // the global value, so the walk degenerates to exactly the historical
    // one-instance-per-plugin shape — the replay/corpus byte-identity
    // posture.
    if config.engine_enabled && config.discovery_enabled {
        let pairs: Vec<(String, i64)> = data_plane
            .discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .iter()
            .cloned()
            .collect();
        // RFC-661 M2: a consumer's (venue, duration) family rides the SOURCE
        // plugin's discovery (one upstream stream). Translate consumer pairs
        // to their source before mounting; the consumer's own plugin supplies
        // no discovery instance (its registrar is the host's fan-out). The
        // translation runs through the DECLARED edges only — a declaration
        // naming nothing compiled-in keeps its own family.
        let resolve_source = |venue: &str| -> String {
            actives
                .iter()
                .find(|p| p.name() == venue)
                .and_then(|p| p.feed_source())
                .filter(|src| registry.get(src).is_some())
                .map(str::to_string)
                .unwrap_or_else(|| venue.to_string())
        };
        let mut pairs: Vec<(String, i64)> = pairs
            .into_iter()
            .map(|(v, d)| (resolve_source(&v), d))
            .collect();
        // The global family sorts FIRST within a venue: the instance every
        // deployment has always run keeps its duration, and the extra families
        // are additions beside that primary, never a replacement of it.
        pairs.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                (a.1 != config.default_round_duration_sec)
                    .cmp(&(b.1 != config.default_round_duration_sec))
                    .then(a.1.cmp(&b.1))
            })
        });
        pairs.dedup();
        let reuse = data_plane.feed_reuse().expect("boot installs the cell");
        // #1165 L3 (自覆盖), applied at BOOT as well as at the runtime mount:
        // a plugin whose discovery walks its own declared family table
        // (`supported_round_secs`) feeds every family in that table from the
        // ONE instance already started — a second per-family instance would
        // double its upstream queries and duplicate every batch. A family
        // OUTSIDE the table (kalshi's [900]) still takes its own instance, and
        // so does every family of a plugin with no table at all (predictfun,
        // binance_spot): those discoveries bake the duration into their config
        // and discover nothing for a family they were not started with.
        let mut covered: std::collections::HashMap<String, Vec<i64>> =
            std::collections::HashMap::new();
        for (venue, duration) in &pairs {
            if covered
                .get(venue)
                .is_some_and(|table| table.contains(duration))
            {
                continue;
            }
            let plugin: std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin> =
                match actives.iter().find(|p| p.name() == *venue) {
                    Some(p) => std::sync::Arc::clone(p),
                    None => {
                        // Consumer-only enablement: the source plugin is not in
                        // `actives` (its executor must NOT start) but its data
                        // components are the upstream every consumer eats —
                        // mount them from the registry (data-plane
                        // self-sufficiency). `resolve_source` already mapped
                        // the consumer venue to its source, so the miss here
                        // IS the consumer-only shape.
                        match registry.get(venue) {
                            Some(p) => p,
                            None => continue,
                        }
                    }
                };
            let Some(disc) = plugin.discovery() else {
                continue;
            };
            let cfg = blitzkrieg_market_api::DiscoveryConfig {
                assets: config.assets.clone(),
                round_duration_sec: *duration,
                poll_sec: 5,
            };
            // The mount-count discipline mirrors the feed loop above: a REUSED
            // stream's discovery mount (a source some consumer declares)
            // counts once per (source, family).
            if reused_sources.contains(venue) {
                reuse
                    .mounts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Err(e) = disc.start(host.clone(), cfg).await {
                eprintln!("blitzkrieg-core: discovery start failed for {venue}@{duration}s: {e}");
            } else {
                if *duration == config.default_round_duration_sec {
                    eprintln!("blitzkrieg-core: discovery started for {venue}");
                } else {
                    eprintln!(
                        "blitzkrieg-core: discovery started for {venue} @ {duration}s family"
                    );
                }
                if let Some(table) = plugin.supported_round_secs() {
                    covered.insert(venue.clone(), table);
                }
            }
        }
        eprintln!("blitzkrieg-core: rust-native round discovery enabled");
    }

    // Live mode: start the market's order executor (CLOB bridge: submits orders,
    // ingests user-WS fills, periodic REST reconciliation). The executor reads
    // credentials from the environment and stays inert when they are absent.
    //
    // This block IS the structural guarantee (E12-e). Dry never places a real
    // order because the egress object is never constructed — there is no
    // `LiveVenue`, no actor loop, and no consumer of `take_pending_orders`, so a
    // pending order simply accumulates in the OME. `--readonly` is therefore not
    // a second guard bolted on top: it refuses entry to this same block, which is
    // why `may_trade()` is the single predicate rather than a chain of conditions
    // each future edit could widen back open.
    if config.mode.may_trade() {
        // Multi-plugin: EVERY selected plugin with an executor starts its own
        // instance. Orders/cancels drain venue-routed by their `plugin`
        // argument; the settlement drains carry no caller name (frozen ABI),
        // so the executor gets a `for_plugin` host handle whose `own_plugin`
        // names ITSELF (#647) — the redemption/merge queues inside the host
        // are venue-bucketed and this handle pulls only its own venue's
        // claims (plus untagged ones, the single-plugin fallback). The shared
        // `active_plugin` cell is NOT touched (#775): it stays the assembly's
        // `actives[0]`, the one owner untagged ORDERS fall back to. Data-plane
        // handles keep the empty own-plugin (legacy all-take); only the
        // executor is named.
        for p in &actives {
            if let Some(exec) = p.executor() {
                // `markets` is the operator's global `--market` condition-id
                // subscription list, not a per-venue allocation — it gates the
                // user-WS subscribe set and is passed through untouched.
                let cfg = blitzkrieg_market_api::ExecutorConfig {
                    markets: config.markets.clone(),
                };
                match exec
                    .start(Arc::new(concrete_host.for_plugin(p.name())), cfg)
                    .await
                {
                    Ok(()) => {
                        eprintln!(
                            "blitzkrieg-core: live order executor started for {}",
                            p.name()
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "blitzkrieg-core: live executor failed to start for {}: {e}",
                            p.name()
                        )
                    }
                }
            }
        }
    } else if config.mode.readonly() {
        eprintln!(
            "blitzkrieg-core: READ-ONLY — venue order egress is not constructed; \
             this process cannot place an order"
        );
    }

    claim_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;
    // Owner-only, immediately after bind (#187). The mode is reported at boot
    // because "the socket exists" says nothing about who may connect to it — the
    // audit found `srwxr-xr-x`, i.e. the full trading control surface readable
    // and writable by every local user.
    let mode = match restrict_socket(&socket_path) {
        Ok(m) => format!("{m:04o}"),
        Err(e) => {
            // Fail LOUD but keep serving: a filesystem that refuses chmod (some
            // network mounts) still gets the peer-uid check below.
            eprintln!(
                "blitzkrieg-core: WARNING could not restrict {socket_path} to {:04o}: {e} — \
                 the socket may be connectable by other local users",
                SOCKET_MODE
            );
            "unknown".to_string()
        }
    };
    eprintln!(
        "blitzkrieg-core listening on {socket_path} mode={:?} version={} socketMode={mode} peerAuth={} logLevel={} dt={} ms",
        config.mode,
        crate::CORE_VERSION,
        peer_auth_support(),
        // #184: the level this process actually records, on the same banner as the
        // socket mode — "was this run quiet by design or by accident?" is
        // answerable from the run log alone.
        crate::logging::effective_level(),
        now_ms().saturating_sub(boot_start)
    );
    // Log the EFFECTIVE risk/exit tuning at boot: a stale binary silently ran the
    // old wide stop for a whole soak; this makes the deployed parameters visible
    // in the run log without trusting that the running file matches current source.
    let ex = &config.positions.exit;
    eprintln!(
        "exit tuning: stop_loss={}% take_profit={}% trailing={} trail_min={}% trail_arm={}% min_time_left={}s force_exit={}s maker_timeout={}ms",
        ex.stop_loss_pct,
        ex.take_profit_pct,
        ex.trailing_enabled,
        ex.min_trail_pct,
        ex.trailing_min_high_pct,
        ex.min_time_left_sec,
        ex.force_exit_sec,
        config.default_maker_timeout_ms
    );

    // Maintenance loop: maker→taker escalation + pending-fill retry, and (when
    // the self-driving engine is enabled) a periodic evaluate→place cycle.
    {
        let core = core.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(tick_ms.max(10)));
            loop {
                interval.tick().await;
                let mut c = core.lock().await;
                if let Err(e) = c.tick(now_ms()) {
                    tracing::warn!(error = %e, "core tick failed");
                }
                if c.has_engine() {
                    let n = c.engine_evaluate(now_ms());
                    if n > 0 {
                        tracing::info!(placed = n, "engine placed entries");
                    }
                }
            }
        });
    }

    // Process-level update state (VERSIONING.md §5.5): deliberately outside
    // `Core`, so version questions never wait behind the trading lock. The
    // initial switches come from the resolved boot config (CLI > env > TOML;
    // built-in default off); the runtime cell persisted in
    // `data/update/state.json` outranks all three and is restored first. The
    // startup check spawns ONLY if the (restored) switch is on — with the
    // shipped defaults this mount is a no-op and the stack makes zero
    // outbound connections.
    let update_state = Arc::new(crate::ipc::version::UpdateState::new(
        config.update_check_enabled,
        config.update_auto,
    ));
    // #379: the automatic-check clock is boot configuration (never written by
    // the UI switches); it rides the same state cell so the scheduler reads it.
    update_state.set_auto_check_interval_secs(config.update_interval_secs);
    crate::ipc::version::load_into(&update_state);
    let github_token = std::env::var("BLITZKRIEG_GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    crate::ipc::version::spawn_startup_check(
        update_state.clone(),
        crate::ipc::build_info::BUILD_INFO.version.to_string(),
        github_token.clone(),
    );
    // #379 (§7.4): the automatic-check scheduler. A no-op task-less return
    // unless the interval is configured AND the check switch is on — with the
    // shipped defaults (interval 0) nothing is spawned and nothing dials.
    // No shutdown wiring on purpose: the task lives inside this runtime, so
    // it ends when the server's process ends — an extra channel would only
    // widen the surface for a lost wakeup.
    let _auto_check = crate::ipc::version::spawn_auto_check_scheduler(
        update_state.clone(),
        crate::ipc::build_info::BUILD_INFO.version.to_string(),
        github_token.clone(),
        std::future::pending(),
    );
    // The `risk.limits` readout (§4.4) is assembled per answer from the
    // LIVE risk config: `risk.setLimits`/`risk.setSystemic` hot-write the
    // matrix, so the arm takes the Core lock and reads it back rather than
    // serving a boot snapshot that a hot edit would make a lie.

    // #353: the backtest job registry — the WebUI's 拉数据→配置→回测→看结果
    // flow runs as tokio tasks INSIDE this process, addressed over IPC. One
    // registry for the whole server; every session's arms share it.
    let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    tokio::pin!(shutdown);
    // Report an unavailable peer check once, not per connection.
    let mut peer_warning_logged = false;
    // P0 #670: accept() 的 Err 臂限速 + 指数退避状态。EMFILE 等错误让
    // accept 立刻返回、select 立刻再就绪 → 紧自旋刷日志（外部用户实测
    // 4 行/11μs，journald+syslog 无上限轮转 7 天写满 50G）。三者都在
    // Ok 臂复位；sleep 只在 Err 臂，shutdown 最多多等一个封顶 2s 的退避。
    let mut accept_err = AcceptErrState::default();
    loop {
        tokio::select! {
            accept = listener.accept() => match accept {
                Ok((stream, _)) => {
                    accept_err.reset_ok();
                    // Authenticate the PEER before serving it (#187): the socket
                    // mode is the file-system control, this is the identity one.
                    let peer = classify_peer(&stream);
                    if !peer.accepted() {
                        // Refused without a single byte read or written, so a
                        // foreign caller learns nothing — not even the protocol.
                        eprintln!(
                            "blitzkrieg-core: refusing IPC connection from {} (this core runs as uid {})",
                            peer.describe(),
                            own_uid()
                        );
                        continue;
                    }
                    if matches!(peer, PeerAuth::Unavailable { .. }) && !peer_warning_logged {
                        peer_warning_logged = true;
                        eprintln!(
                            "blitzkrieg-core: WARNING {} — falling back to the socket file mode alone",
                            peer.describe()
                        );
                    }
                    spawn_session(
                        stream,
                        peer,
                        core.clone(),
                        bus_tx.subscribe(),
                        registry.clone(),
                        update_state.clone(),
                        jobs.clone(),
                        host.clone(),
                        data_plane.clone(),
                    )
                }
                Err(e) => {
                    let now = std::time::Instant::now();
                    let kind = e.kind();
                    let (should_log, suppressed) = accept_err.step(kind, now);
                    if should_log {
                        tracing::warn!(error = %e, suppressed, "accept failed");
                    }
                    // P0 #670: 命中 EMFILE 才广播 RiskAlert，且只在限速窗
                    // 放行的那次（首命中必放行；其后 5s 内静默，绝不给自旋
                    // 期间的每次失败各发一条）。
                    if e.raw_os_error() == Some(ACCEPT_EMFILE_ERRNO) && should_log {
                        let c = core.lock().await;
                        c.emit_risk_alert(
                            CoreErrorCode::Internal,
                            format!(
                                "IPC accept hit EMFILE (fd exhaustion): open as many files as \
                                 allowed? suppressed {suppressed} repeat error(s) before this \
                                 alert; details in the log (issue #670)"
                            ),
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        accept_err.backoff_ms,
                    ))
                    .await;
                }
            },
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = &mut shutdown => break,
        }
    }
    // Persist any pending near-miss records before exiting.
    core.lock().await.flush_near_misses();
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// 单条 IPC 行的最大字节数：客户端发超大、不带换行的一行不得把读
/// 缓冲区撑爆；超限即断开该连接（issue #1199 P1-20）。
/// #1282 已落地手动累积读循环：无换行残留超过本上限即截断断开——
/// 恶意方发 GB 级无换行流最多也只会缓冲 1MB。
// pub(super)：tests/ 的超长行用例直接引用上限断言行为
pub(super) const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 出站通道容量（按条数，#1282 契约 1）。此前 unbounded 在慢/离线客户端
/// 下无限增长（#1199 P1-19 曾试过 bounded(256)+send().await，因阻塞读
/// 循环丢请求被回退——现在两类消息分流，见响应/通知两处注释）。
/// 内存上界估算：1024 条 × 单条 JSON（通知 KB 级；响应受 MAX_LINE_BYTES
/// 1MB 约束的最坏情形仅理论存在，典型快照几十 KB）——典型几 MB、
/// 理论上界 ~1GB，但关键是「有界」：最坏情形不再随时间无限增长。
const OUTBOUND_QUEUE_CAP: usize = 1024;

/// 写端单次 write_all / 响应入队的等待上限（#1282 契约 2）。超时判定
/// 客户端不可达，只断开该会话——每会话独立任务，绝不无界阻塞，也不
/// 拖累其他会话。30s：正常面板/网关的写抖动远小于此，卡死客户端不该
/// 被无限等待。
const WRITE_STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// 读空闲超时：600s 内一个字节都收不到即断开。要远大于合法客户端的
/// 请求间隔（面板是常驻长连接），又不能永远等——半开连接（对端掉电/
/// 网络静默丢弃）不会有 RST，只能靠超时回收，否则断连的会话随运行
/// 时间无限累积。
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// 满队列丢通知的限频告警节奏：首次丢弃即报，之后每丢 100 条报一次。
/// 慢客户端会持续触发 Full，逐条告警会把日志刷爆（#670 的自旋教训）。
const NOTIFY_DROP_LOG_EVERY: u64 = 100;

/// `pub(super)` 仅为 tests/ 开放：背压语义（多行单 read、满队列丢通知）
/// 只有对真实 socketpair 打满整个会话任务才观察得到，handle_line 级
/// 的 rpc 助手够不着这些任务（#1282）。
#[allow(clippy::too_many_arguments)] // session wiring — each param has one job
pub(super) fn spawn_session(
    stream: UnixStream,
    peer: PeerAuth,
    core: Arc<AsyncMutex<Core>>,
    events: broadcast::Receiver<Event>,
    registry: crate::market::registry::MarketPluginRegistry,
    update_state: Arc<crate::ipc::version::UpdateState>,
    jobs: Arc<crate::backtest_jobs::BacktestJobs>,
    host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    data_plane: DataPlaneRuntime,
) {
    tokio::spawn(async move {
        let (mut read_half, write_half) = stream.into_split();
        // #1282 契约 1：出站通道有界化。unbounded 在慢/离线客户端下让
        // 出站缓冲无限增长；bounded 后两类消息分流——响应走带
        // WRITE_STALL 超时的 send（满则等写端，等不到断开，绝不丢
        // 响应），通知走 try_send（满即丢，见通知任务注释）。
        let (out_tx, mut out_rx) = mpsc::channel::<String>(OUTBOUND_QUEUE_CAP);

        let writer = tokio::spawn(async move {
            let mut w = write_half;
            while let Some(line) = out_rx.recv().await {
                // #1282 契约 2：写端超时。write_all 卡满 WRITE_STALL（对端
                // 不读、半开连接）即判客户端不可达，只断开本会话；写失败
                // （EPIPE 等）同断。绝不无界阻塞内核或其他会话。
                match tokio::time::timeout(WRITE_STALL, w.write_all(line.as_bytes())).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
        });

        // The connection-scoped state (§9.5 + §12.2): the session's default
        // account (`account.switch` writes THIS cell — never Core's
        // process-level one, so two connections can look at two accounts side
        // by side) and the session's K-line subscription set (§12.2) —
        // written by this connection's `kline.subscribe`/`kline.unsubscribe`
        // arms, read by the event-forwarding task below, and dropped with the
        // connection, which IS the auto-unsubscribe (a closed panel cannot
        // keep receiving market pushes; a reconnect starts clean, so no
        // double-push).
        let mut session = SessionState::default();

        // Server -> client notifications.
        // 句柄留给会话收场：连接一断（读循环退出），通知转发任务必须随
        // 之 abort——它持有 out_tx 克隆，不终止则 writer 等 senders 归零、
        // 会话任务等 writer，读半端永不 drop，死会话的 socket 拖到
        // shutdown 才回收（断连即退订的字面落实，#1282）。
        let notifier = {
            let out_tx = out_tx.clone();
            let session_subs = session.kline_subs.clone();
            tokio::spawn(async move {
                let mut events = events;
                // #1282 契约 1：满队列丢通知不丢响应。通知是 best-effort 的
                // 推送面（行情/事件），慢客户端不该让它拖垮内核——满即丢
                // 并计数，首次及每 100 条限频告警一次；队列已关（会话已断）
                // 则任务退出。决不阻塞等待。
                let mut dropped: u64 = 0;
                loop {
                    match events.recv().await {
                        Ok(ev) => {
                            // KLINE_UPDATE is the one event a session must
                            // OPT INTO (§12.2): an unfiltered broadcast would
                            // push every bucket's preview to every client —
                            // nine intervals × every symbol at 1/s each.
                            if let Event::KlineUpdate { kline } = &ev {
                                let subscribed = session_subs
                                    .lock()
                                    .map(|subs| {
                                        subs.contains(&(kline.symbol.clone(), kline.interval))
                                    })
                                    .unwrap_or(false);
                                if !subscribed {
                                    continue;
                                }
                            }
                            if let Ok(json) = serde_json::to_string(&Notification::new(ev)) {
                                match out_tx.try_send(format!("{json}\n")) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        dropped += 1;
                                        if dropped == 1
                                            || dropped.is_multiple_of(NOTIFY_DROP_LOG_EVERY)
                                        {
                                            tracing::warn!(
                                                dropped,
                                                "lagged notifications dropped: outbound \
                                                 queue full (slow IPC client)"
                                            );
                                        }
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
        };

        // #1282 契约 3：读路径手动累积 + 预限长 + 读超时，替换
        // BufReader::lines()。三条硬约束：
        // 1) P1-12b 的教训：一次 read() 带出的多行必须全部按序处理，
        //    剩余字节必须保留（drain 到 \n 为止、余量留在 pending）——
        //    自定义循环曾因丢 leftover 无声丢掉并发连发的第 3 个请求，
        //    CI risk-systemic-check 红过一次，本行为由 tests_backpressure
        //    的多行单 read 用例钉死；
        // 2) 无换行残留超过 MAX_LINE_BYTES 即截断断开，不缓冲无限长单行
        //    （单行超限完整到达时同样断开——不再处理超长行，P1-20 语义）；
        // 3) READ_IDLE_TIMEOUT 内一个字节都收不到即断开：半开连接没有
        //    RST，不回收的会话会随断连客户端无限累积。
        // 非法 UTF-8 → 断开（lines() 的 Err → break 语义）；空/纯空白行
        // → 断开（基线 lines() 对 Ok(Some("")) 的 break 语义）。
        let mut pending: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut chunk = [0u8; 8 * 1024];
        'session: loop {
            let n = match tokio::time::timeout(READ_IDLE_TIMEOUT, read_half.read(&mut chunk)).await
            {
                Ok(Ok(0)) | Err(_) => break 'session, // EOF / 读空闲超时
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break 'session, // 读错误（对端已关）
            };
            pending.extend_from_slice(&chunk[..n]);
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let mut line_bytes: Vec<u8> = pending.drain(..=pos).collect();
                line_bytes.pop(); // \n 已定位，pop 必命中
                if line_bytes.last() == Some(&b'\r') {
                    line_bytes.pop(); // lines() 同样吞 \r\n
                }
                if line_bytes.len() > MAX_LINE_BYTES {
                    break 'session; // 超长行断开（P1-20）
                }
                let line = match String::from_utf8(line_bytes) {
                    Ok(line) => line,
                    Err(_) => break 'session,
                };
                if line.trim().is_empty() {
                    break 'session;
                }
                let response = handle_line(
                    &core,
                    &registry,
                    line,
                    &peer,
                    &update_state,
                    &jobs,
                    &host,
                    &data_plane,
                    &mut session,
                )
                .await;
                // 响应路径（契约 1）：绝不丢响应。满队列时最多等
                // WRITE_STALL——写端 30s 还消费不掉即客户端不可达，断开
                // 该会话降级；通道关闭（写端已死）同断。
                let sent =
                    tokio::time::timeout(WRITE_STALL, out_tx.send(format!("{response}\n"))).await;
                match sent {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            "IPC session response channel closed — tearing down session (write fail-closed)"
                        );
                        break 'session;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "IPC session write stalled past WRITE_STALL — client unreachable, tearing down session (write fail-closed)"
                        );
                        break 'session;
                    }
                }
            }
            // 无换行残留超限：截断（清空，不再喂给解析）并断开（契约 3）。
            if pending.len() > MAX_LINE_BYTES {
                pending.clear();
                break 'session;
            }
        }
        drop(out_tx);
        notifier.abort();
        let _ = writer.await;
    });
}
