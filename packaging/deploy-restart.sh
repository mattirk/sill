#!/bin/sh
# Internal post-install phase of deploy.sh; no privileged operations here.
set -eu
cd "$(dirname "$0")/.."
PREFIX=$1
sill_build_dir=$2
SILL_BOX=local; export SILL_BOX
# Runs in a separate user service so stopping sill cannot kill deployment verification.
"$PREFIX/bin/sill" restart --jails --detached

# Verify the daemon process itself, not merely the executable on disk.
sill_daemon_status=$("$PREFIX/bin/sill" daemon status)
sill_daemon_pid=$(printf '%s\n' "$sill_daemon_status" | sed -n 's/.*pid \([0-9][0-9]*\),.*/\1/p')
case "$sill_daemon_pid" in
    ''|*[!0-9]*) echo "deploy.sh: cannot identify replacement daemon: $sill_daemon_status" >&2; exit 1 ;;
esac
cmp -s "/proc/$sill_daemon_pid/exe" "$PREFIX/bin/sill" || {
    echo "deploy.sh: daemon $sill_daemon_pid is not running the installed build" >&2
    exit 1
}

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
