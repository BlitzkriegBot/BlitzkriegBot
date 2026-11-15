import re
from pathlib import Path

conflict_files = [
    'core/blitzkrieg_core/src/ipc/server/session.rs',
    'core/blitzkrieg_core/src/ipc/server/tests/tests_account_settings_write.rs',
    'core/blitzkrieg_core/src/ipc/server/tests/tests_risk_rounds.rs',
    'core/blitzkrieg_core/src/risk/account_store.rs',
    'core/blitzkrieg_core/src/service/assembly.rs',
    'core/blitzkrieg_core/src/service/tests/strategy_dispatch_tests/dispatch_boot_enablement_tests.rs',
    'core/blitzkrieg_core/src/service/trading_reset.rs',
    'scripts/file-size-check.mjs',
]

resolutions = {
    'core/blitzkrieg_core/src/ipc/server/session.rs': 'theirs',
    'core/blitzkrieg_core/src/ipc/server/tests/tests_account_settings_write.rs': 'theirs',
    'core/blitzkrieg_core/src/ipc/server/tests/tests_risk_rounds.rs': 'ours',
    'core/blitzkrieg_core/src/risk/account_store.rs': 'theirs',
    'core/blitzkrieg_core/src/service/assembly.rs': 'manual',
    'core/blitzkrieg_core/src/service/tests/strategy_dispatch_tests/dispatch_boot_enablement_tests.rs': 'ours',
    'core/blitzkrieg_core/src/service/trading_reset.rs': 'manual',
    'scripts/file-size-check.mjs': 'manual',
}

pattern = re.compile(
    r'^<<<<<<< HEAD\n(.*?)\n^=======\n(.*?)\n^>>>>>>> ceer/main\n',
    re.MULTILINE | re.DOTALL
)

for f in conflict_files:
    content = Path(f).read_text()
    strategy = resolutions.get(f, 'manual')
    
    if strategy == 'theirs':
        new_content = pattern.sub(r'\2', content)
        Path(f).write_text(new_content)
        print(f'{f}: resolved to theirs (ceer/main)')
    elif strategy == 'ours':
        new_content = pattern.sub(r'\1', content)
        Path(f).write_text(new_content)
        print(f'{f}: resolved to ours (PR branch)')
    elif strategy == 'manual':
        print(f'{f}: MANUAL resolution needed - {len(list(pattern.finditer(content)))} blocks')
