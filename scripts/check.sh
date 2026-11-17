#!/bin/sh
# The checks a change must pass, the same here and in CI (.github/workflows/build.yml calls this).
# Usage: scripts/check.sh [PACKAGES...]     default: --workspace
#   scripts/check.sh                        everything
#   scripts/check.sh -p sill-core           one crate, while working on it
# In the Mac app's Linux jail: scripts/dev-jail.sh scripts/check.sh [PACKAGES...]
#
# Steps: formatting, Clippy (a warning fails), the release scripts' tests, the Rust tests. A step
# that fails stops the run.
# Tests marked #[ignore] need something a plain machine lacks (their reason says what) and are
# not run here.
set -eu
cd "$(dirname "$0")/.."
[ $# -gt 0 ] || set -- --workspace

# The daemon tests make a socket private under the build directory; a folder shared from the Mac
# (virtiofs) refuses that chmod and every such test fails (docs/dev-jail.md). Build elsewhere.
if [ -z "${CARGO_TARGET_DIR:-}" ] && [ "$(uname -s)" = Linux ] && [ "$(stat -f -c %T . 2>/dev/null)" = virtiofs ]; then
    export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/sill-dev/target"
    echo "check: build output in $CARGO_TARGET_DIR (the repository is on a shared folder)"
fi

step() { echo "== $*"; "$@"; }
step cargo fmt --all --check
step cargo clippy --locked "$@" --all-targets -- -D warnings
# Mac-only code is not compiled on Linux. Where the Mac target is installed
# (`rustup target add aarch64-apple-darwin`), lint the crates the Mac builds (build.yml's macOS
# packages) for it too: Clippy only checks, so no Apple SDK is needed.
if [ "$(uname -s)" = Linux ] && [ "$*" = --workspace ] && rustup target list --installed 2>/dev/null | grep -qx aarch64-apple-darwin; then
    step cargo clippy --locked --target aarch64-apple-darwin -p sill-gui -p sill-vm -p sill-client -p sill-proto --all-targets -- -D warnings
fi
step python3 packaging/test_release.py
# Tests run in parallel, as many at once as the machine has cores. SILL_TEST_THREADS=1 runs them
# one at a time: for telling a test that fails from one that fails only beside others.
if [ -n "${SILL_TEST_THREADS:-}" ]; then
    step cargo test --locked "$@" -- --test-threads="$SILL_TEST_THREADS"
else
    step cargo test --locked "$@"
fi
echo "== all checks passed"
