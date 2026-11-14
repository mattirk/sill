#!/bin/sh
# Build, install and restart sill on this machine, then check that the shelf on screen is the build
# just made. Run it in a host terminal (Ctrl+Shift+H) — not in a session's shell: sudo is refused
# inside the jail by design (SPEC §4.2).
#
#   packaging/deploy.sh             # cargo build --release, sudo sill init, sill restart, verify,
#                                   # then packaging/check.sh (report in target/host-check/)
#   packaging/deploy.sh --no-build  # install and restart what target/release already has
set -eu
cd "$(dirname "$0")/.."
if [ -d /.sill ]; then
    echo "deploy.sh: this is a sill jail; run it in a host terminal (Ctrl+Shift+H)" >&2
    exit 1
fi
# Deployment and host checks always address this machine, even from a shell with SILL_BOX set.
SILL_BOX=local; export SILL_BOX
PREFIX=${PREFIX:-/usr/local}
sudo -v # the password now, not after the build
if [ "${1:-}" != "--no-build" ]; then
    cargo build --release
fi
for b in sill sill-shim; do
    [ -x "target/release/$b" ] || { echo "deploy.sh: target/release/$b missing" >&2; exit 1; }
done

sudo target/release/sill init --prefix "$PREFIX"
# what init installed must be what was built
for b in sill sill-shim; do
    cmp -s "target/release/$b" "$PREFIX/bin/$b" || {
        echo "deploy.sh: $PREFIX/bin/$b differs from target/release/$b after init" >&2
        exit 1
    }
done

# restart: stops the unit, quits any other shelf holding the application id, starts the unit
"$PREFIX/bin/sill" restart

# exactly one local shelf, the installed one, under the unit
sleep 2
shelves=$(packaging/local-shelves.sh)
n=$(printf '%s\n' "$shelves" | grep -c . || true)
if [ "$n" -ne 1 ]; then
    echo "deploy.sh: expected one local shelf process, found $n:" >&2
    printf '%s\n' "$shelves" >&2
    exit 1
fi
sill_shelf_pid=${shelves%% *}
sill_shelf_exe=$(readlink "/proc/$sill_shelf_pid/exe" 2>/dev/null || true)
sill_expected_exe=$(readlink -f "$PREFIX/bin/sill")
case "$sill_shelf_exe" in
    "$sill_expected_exe") ;;
    *) echo "deploy.sh: the running local shelf is not $PREFIX/bin/sill: $shelves" >&2; exit 1 ;;
esac
if command -v systemctl >/dev/null 2>&1 && systemctl --user cat sill-shelf.service >/dev/null 2>&1; then
    systemctl --user is-active --quiet sill-shelf.service || {
        echo "deploy.sh: sill-shelf.service is not active (journalctl --user -u sill-shelf)" >&2
        exit 1
    }
fi
echo "deployed: $PREFIX/bin/sill $(sha256sum "$PREFIX/bin/sill" | cut -c1-12) running as pid ${shelves%% *}"

# what a session cannot see from its jail, into target/host-check/report.txt for the agent to read
PREFIX="$PREFIX" packaging/check.sh || {
    echo "deploy.sh: deployed, but check.sh found $? failure(s): target/host-check/report.txt" >&2
    exit 1
}
