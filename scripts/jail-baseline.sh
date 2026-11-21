#!/bin/sh
# One-off, to be removed again: do the four jail tests that fail in the Windows VM's guest
# (docs/silli-windows.md, section 4.1) pass on a Linux host today? They are ignored by default,
# so nothing has said. Run as yourself on the Linux machine, outside any Silli jail:
#
#   scripts/jail-baseline.sh
#
# and report the lines it prints at the end.
set -u
cd "$(dirname "$0")/.."
log=$(mktemp)
cargo test -p silli-core --lib -- --ignored real_agent_storage login_copy_tests >"$log" 2>&1
status=$?
echo "== $(uname -sr), bubblewrap $(bwrap --version 2>/dev/null | cut -d' ' -f2), commit $(git rev-parse --short HEAD 2>/dev/null)"
grep -E '^test .* \.\.\. |^test result|panicked at' "$log"
# why each failed, one line
grep -A1 'panicked at' "$log" | grep -vE 'panicked at|^--$' | cut -c1-200
echo "== exit $status (0: all four pass on this machine)"
rm -f "$log"
