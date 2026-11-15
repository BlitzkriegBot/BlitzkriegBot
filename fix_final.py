from pathlib import Path
import re

# session.rs: HEAD has `let mut core = Core::new(config.clone());`, ceer/main has full setup
# Take ceer/main (more complete)
fp = Path('/tmp/pr1230-check/core/blitzkrieg_core/src/ipc/server/session.rs')
c = p = Path(filepath).read_text() if False else None
# Just do it manually with exact string match
content = Path(filepath).read_text()
old = """        <<<<<<< HEAD
    let mut core = Core::new(config.clone());
=======
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
    // launched from, while the deployment's real database sat unread).
    let db_file_preexisted = db_path.exists();
    let db_parent_preexisted = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.is_dir())
        .unwrap_or(true);
    if let Err(e) = core.open_account_risk_db(db_path.clone()) {
"""
new = """        use crate::risk::account_store::{ROUND_DURATION_SETTING, STRATEGIES_SETTING};
        let settings = db
            .load_settings()
            .map_err(|e| format!("account_settings read failed: {e}"))?;
        let default_id = crate::model::default_account_id();
        for (account_id, keys) in &settings {
            let account_id_owned = account_id.clone();
            let id = blitzkrieg_market_api::AccountId::from(account_id.as_str());
            // Round anchor: only applied to accounts that exist in the book.
            if let Some(raw) = keys.get(ROUND_DURATION_SETTING) {
                let sec: i64 = match raw.trim().parse() {
                    Ok(sec) => sec,
                    Err(_) => {
                        tracing::warn!(
                            account = %account_id,
                            value = %raw,
                            "account_settings round anchor is not an integer — the boot-time \
                             env value stands for this account"
                        );
                        continue;
                    }
                };
                if !self.config.allowed_round_secs.contains(&sec) {
                    return Err(format!(
                        "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is \
                         not declared by any market plugin (declared union: {:?}) — fix the \
                         account_settings row or add the duration to a plugin",
                        self.config.allowed_round_secs
                    ));
                }
                if let Some(acc) = self.accounts.account_mut(&id) {
                    let subscribed = acc.rounds.clone();
                    if !subscribed.is_empty() && !subscribed.contains(&sec) {
                        return Err(format!(
                            "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is \
                             not in its subscribed round families ({subscribed:?}) — fix the \
                             account_settings row or the account's ROUNDS"
                        ));
                    }
                }
                // 操作者裁决：per-account 覆盖与进程默认网格分离。default 账户的
                // 覆盖只进 account_round_durations，不改 default_round_duration_sec
                // （gate 合同：effectiveSec 恒为出厂默认 900，perAccount.default=300）。
                self.config
                    .account_round_durations
                    .insert(account_id_owned.clone(), sec);
            }
"""

if old in content:
    content = content.replace(old, new)
    Path(filepath).write_text(content)
    print('session.rs: resolved')
else:
    print('session.rs: pattern not found')
    # Show what's actually there
    lines = content.split('\\n')
    for i in range(75, 80):
        print(f'{i+1}: {repr(lines[i])}')
