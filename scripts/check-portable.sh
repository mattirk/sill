#!/bin/sh
# Non-Linux compile checks for the frontend client core (docs/silli-gui.md, implementation
# direction 3: Linux delivery first, a portable core). Two parts:
#
# 1. A static guard that runs anywhere: silli-client's own code (tests excepted) uses no
#    OS-specific API — no std::os::*, rustix, libc or /proc.
# 2. `cargo check -p silli-client` for macOS and Windows targets whose standard library is
#    installed (`rustup target add aarch64-apple-darwin x86_64-pc-windows-gnu`). A target that
#    is not installed is reported as skipped, never as passed.
#
# Known today: silli-proto, which silli-client builds on, is Unix-only (its local socket transport,
# exact path bytes through OsStrExt, file modes); the Windows check fails there until those parts
# are put behind cfg(unix) with the SSH stdio transport as Windows' way in. The list is printed.
set -u
cd "$(dirname "$0")/.." || exit 2
status=0

echo "== client core: OS-specific code outside tests"
found=$(grep -n 'std::os::\|rustix\|libc::\|/proc/' crates/silli-client/src/*.rs \
    | grep -v '^crates/silli-client/src/\(fake\|files_tests\)\.rs:' \
    | grep -v '^crates/silli-client/src/terminal\.rs:.*use std::os::unix::net::UnixStream;')
if [ -n "$found" ]; then
    echo "$found"
    echo "FAIL: silli-client must stay free of OS-specific code"
    status=1
else
    echo "ok"
fi

echo "== silli-proto: Unix-only code (Windows blockers)"
grep -n 'std::os::unix\|rustix::' crates/silli-proto/src/*.rs | sed 's/^/  /'

sysroot=$(rustc --print sysroot)
for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-pc-windows-gnu x86_64-pc-windows-msvc; do
    if [ ! -d "$sysroot/lib/rustlib/$target/lib" ]; then
        echo "== $target: skipped (standard library not installed)"
        continue
    fi
    echo "== $target: cargo check -p silli-client"
    if cargo check -q -p silli-client --target "$target"; then
        echo "ok"
    else
        echo "FAIL: $target"
        status=1
    fi
done
exit $status
