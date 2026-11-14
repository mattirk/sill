#!/bin/sh
# Build, install and restart sill on this machine, then check that the window on screen is the build
# just made. Run it in a host terminal (Ctrl+Shift+H) — not in a session's shell: sudo is refused
# inside the jail by design (SPEC §4.2).
#
#   packaging/deploy.sh             # cargo build --release, sudo sill init, sill restart, verify,
#                                   # then packaging/check.sh (report in target/host-check/)
#   packaging/deploy.sh --fast      # faster incremental build in target/local, then deploy
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
sill_profile=release
sill_build=yes
for arg in "$@"; do
    case "$arg" in
        --fast) sill_profile=local ;;
        --no-build) sill_build=no ;;
        *) echo "usage: packaging/deploy.sh [--fast] [--no-build]" >&2; exit 1 ;;
    esac
done
sill_build_dir="target/$sill_profile"
sudo -v # the password now, not after the build
if [ "$sill_build" = yes ]; then
    # the workspace: sill, sill-shim and sill-gui
    cargo build --profile "$sill_profile"
fi
for b in sill sill-shim sill-gui; do
    [ -x "$sill_build_dir/$b" ] || { echo "deploy.sh: $sill_build_dir/$b missing" >&2; exit 1; }
done

# init installs the three binaries from next to the one that runs it
sudo "$sill_build_dir/sill" init --prefix "$PREFIX"
# what init installed must be what was built
for b in sill sill-shim sill-gui; do
    cmp -s "$sill_build_dir/$b" "$PREFIX/bin/$b" || {
        echo "deploy.sh: $PREFIX/bin/$b differs from $sill_build_dir/$b after init" >&2
        exit 1
    }
done

# restart: stops the unit, ends any other sill-gui, restarts the daemon, starts the unit
"$PREFIX/bin/sill" restart

# exactly one local sill-gui, the installed one, under the unit
sleep 2
windows=$(packaging/local-gui.sh)
n=$(printf '%s\n' "$windows" | grep -c . || true)
if [ "$n" -ne 1 ]; then
    echo "deploy.sh: expected one local sill-gui process, found $n:" >&2
    printf '%s\n' "$windows" >&2
    exit 1
fi
sill_gui_pid=${windows%% *}
sill_gui_exe=$(readlink "/proc/$sill_gui_pid/exe" 2>/dev/null || true)
sill_expected_exe=$(readlink -f "$PREFIX/bin/sill-gui")
case "$sill_gui_exe" in
    "$sill_expected_exe") ;;
    *) echo "deploy.sh: the running sill-gui is not $sill_expected_exe: $windows" >&2; exit 1 ;;
esac
if command -v systemctl >/dev/null 2>&1 && systemctl --user cat sill-gui.service >/dev/null 2>&1; then
    systemctl --user is-active --quiet sill-gui.service || {
        echo "deploy.sh: sill-gui.service is not active (journalctl --user -u sill-gui)" >&2
        exit 1
    }
fi
echo "deployed: $PREFIX/bin/sill $(sha256sum "$PREFIX/bin/sill" | cut -c1-12); sill-gui $sill_expected_exe running as pid $sill_gui_pid"

# what a session cannot see from its jail, into target/host-check/report.txt for the agent to read
PREFIX="$PREFIX" SILL_BUILD_DIR="$sill_build_dir" packaging/check.sh || {
    echo "deploy.sh: deployed, but check.sh found $? failure(s): target/host-check/report.txt" >&2
    exit 1
}
