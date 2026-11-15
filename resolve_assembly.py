from pathlib import Path

filepath = 'core/blitzkrieg_core/src/service/assembly.rs'
content = Path(filepath).read_text()

pattern = re.compile(
    r'^<<<<<<< HEAD\n(.*?)\n^=======\n(.*?)\n^>>>>>>> ceer/main\n',
    re.MULTILINE | re.DOTALL
)

m = pattern.search(content)
if not m:
    print("No conflict found in assembly.rs")
    exit(1)

head = m.group(1)
main = m.group(2)

merged = '''        use crate::risk::account_store::{ROUND_DURATION_SETTING, STRATEGIES_SETTING};
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

new_content = content[:m.start()] + merged + content[m.end():]
Path(filepath).write_text(new_content)
print(f'{filepath}: assembly.rs resolved')
