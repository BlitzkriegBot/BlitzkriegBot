from pathlib import Path
import re

def resolve_file(filepath, strategy='theirs'):
    content = Path(filepath).read_text()
    pattern = re.compile(
        r'^<<<<<<< HEAD\n(.*?)\n^=======\n(.*?)\n^>>>>>>> ceer/main\n',
        re.MULTILINE | re.DOTALL
    )
    
    def replacer(m):
        if strategy == 'theirs':
            return m.group(2)
        elif strategy == 'ours':
            return m.group(1)
        return m.group(0)
    
    new_content = pattern.sub(replacer, content)
    Path(filepath).write_text(new_content)
    remaining = pattern.findall(new_content)
    if remaining:
        print(f'{filepath}: still has {len(remaining)} conflicts')
    else:
        print(f'{filepath}: resolved')

# Resolve remaining conflicts
resolve_file('core/blitzkrieg_core/src/ipc/server/session.rs', 'theirs')
resolve_file('core/blitzkrieg_core/src/ipc/server/tests/tests_account_settings_write.rs', 'ours')
