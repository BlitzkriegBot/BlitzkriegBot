import re
from pathlib import Path

# Resolve trading_reset.rs - keep HEAD version (ours) for both conflict blocks
filepath = 'core/blitzkrieg_core/src/service/trading_reset.rs'
content = Path(filepath).read_text()

# Conflict 1: lines around 727-756
pattern1 = re.compile(
    r'^<<<<<<< HEAD\n        // 操作者裁决 2026-11-14：轮次落 SQLite `account_settings` \(key=round.duration_sec\)，不再写 `\.env`；持久优先失败即拒绝，无库仅内存生效。\n        if let Some\(db\) = self\.account_risk_db\.clone\(\) \{\n            let value = sec\.map\(\|s\| s\.to_string\(\)\);\n            db\.save_setting\(\n                account_id,\n                crate::risk::account_store::ROUND_DURATION_SETTING,\n                value\.as_deref\(\),\n            \)\n            \.map_err\(|e\| {\n                CoreError::new\(\n                    CoreErrorCode::InvalidParams,\n                    format\!("round_duration\.set: SQLite 落盘失败，未做任何变更 \(=\{e\}"\)),\n                \)\n            \)\);\n        } else \{\n            tracing::warn!\(target: "risk", account = %account_id,\n                "round_duration\.set: no SQLite store — grid moves for this session only, nothing persisted"\);\n        }\n=======\n        // 操作者裁决 2026-11-14：轮次和账户其它数据同源 —— 落 SQLite\n        // \(`account_settings\.round\.duration_sec`\)，\*\*不再写 `\.env`\*\*。\n        // 持久优先：有库时先落库，失败即拒绝且无任何账簿副作用；无库\n        // \(回测 / 进程内测试\) 只有内存生效，日志如实说明未落盘 ——\n        // 任何情况下都不写文件，`\.env` 是只读的，SQLite\n        // 是唯一持久化后端。\n        // 再撤单→市价强平\(fail-closed\)，最后内存更新\+引擎网格切换。\n        // 任何一步失败都不留半边天。\n        // `persisted` 反映写入是否落到了 SQLite\(`\.env` 是只读的，SQLite\n        // 是唯一持久化后端\)。\n        let persisted = self\.account_risk_db\.is_some\(\);\n        if let Some\(db\) = self\.account_risk_db\.clone\(\) \{\n            let durable = if patch\.reset \{\n                None\n            \} else {\n                sec\.map\(\|s\| s\.to_string\(\)\)\n            \};\n            db\.save_setting\(\n                account_id,\n                crate::risk::account_store::ROUND_DURATION_SETTING,\n                durable\.as_deref\(\)\n            \)\n>>>>>>> ceer/main',
        r'        // 操作者裁决 2026-11-14：轮次落 SQLite `account_settings`（key=round.duration_sec），不再写 `.env`；持久优先失败即拒绝，无库仅内存生效。\n        if let Some(db) = self.account_risk_db.clone() {\n            let value = sec.map(|s| s.to_string());\n            db.save_setting(\n                account_id,\n                crate::risk::account_store::ROUND_DURATION_SETTING,\n                value.as_deref(),\n            )'
        )
        
# ... this is getting too complex with regex. Let me just do it line by line.

print("Done - manual editing needed")