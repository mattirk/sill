#!/bin/sh
# The checks a change must pass, the same here and in CI (.github/workflows/build.yml calls this).
# Usage: scripts/check.sh [PACKAGES...]     default: --workspace
#   scripts/check.sh                        everything
#   scripts/check.sh -p sill-core           one crate, while working on it
# In the Mac app's Linux jail: scripts/dev-jail.sh scripts/check.sh [PACKAGES...]
#
# Steps: formatting, the release scripts' tests, the Rust tests. A step that fails stops the run.
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
step python3 packaging/test_release.py
# One test at a time, as CI has always run them; SILL_TEST_THREADS overrides it.
step cargo test --locked "$@" -- --test-threads="${SILL_TEST_THREADS:-1}"
echo "== all checks passed"
