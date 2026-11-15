from pathlib import Path

# File 1: assembly.rs - merge HEAD and ceer/main versions
filepath = '/tmp/pr1230-check/core/blitzkrieg_core/src/service/assembly.rs'
content = Path(filepath).read_text()

old = '''        use crate::risk::account_store::ROUND_DURATION_SETTING;
        let settings = db
            .load_settings()
            .map_err(|e| format!("account_settings read failed: {e}"))?;
        let default_id = crate::model::default_account_id();
        for (account_id, keys) in &settings {
            let Some(raw) = keys.get(ROUND_DURATION_SETTING) else {
                continue;
            };
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
            let subscribed = self
                .accounts
                .account(&blitzkrieg_market_api::AccountId::from(account_id.as_str()))
                .map(|a| a.rounds.clone())
                .unwrap_or_default();
            if !subscribed.is_empty() && !subscribed.contains(&sec) {
                return Err(format!(
                    "refusing to boot: account `{account_id}`'s stored round anchor {sec}s is \
                     not in its subscribed round families ({subscribed:?}) — fix the \
                     account_settings row or the account's ROUNDS"
                ));
            }
            if account_id == default_id.as_str()
                && self.config.round_duration_source == "cli_or_env"
            {
                tracing::warn!(
                    stored = sec,
                    effective = self.config.default_round_duration_sec,
                    "account_settings holds a default-account round anchor, but the grid is \
                     locked by an explicit CLI/env flag — the flag stands"
                );
            }
            // 操作者裁决：per-account 覆盖与进程默认网格分离。default 账户的
            // 覆盖只进 account_round_durations，不改 default_round_duration_sec
            // （gate 合同：effectiveSec 恒为出厂默认 900，perAccount.default=300）。
            self.config
                .account_round_durations
                .insert(account_id.clone(), sec);
'''

new = '''        use crate::risk::account_store::{ROUND_DURATION_SETTING, STRATEGIES_SETTING};
        let settings = db
            .load_settings()
            .map_err(|e| format!("account_settings read failed: {e}"))?;
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
'''

if old in content:
    content = content.replace(old, new)
    Path(filepath).write_text(content)
    print('assembly.rs: resolved')
else:
    print('assembly.rs: pattern not found - checking content...')
    # Print what's around line 87
    lines = content.split('\n')
    for i in range(84, 95):
        print(f'{i+1}: {lines[i]}')
