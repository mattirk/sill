#!/bin/sh
# Internal post-install phase of deploy.sh; no privileged operations here.
set -eu
cd "$(dirname "$0")/.."
PREFIX=$1
silli_build_dir=$2
SILLI_BOX=local; export SILLI_BOX
# Runs in a separate user service so stopping silli cannot kill deployment verification.
"$PREFIX/bin/silli" restart --jails --detached

# Verify the daemon process itself, not merely the executable on disk.
silli_daemon_status=$("$PREFIX/bin/silli" daemon status)
silli_daemon_pid=$(printf '%s\n' "$silli_daemon_status" | sed -n 's/.*pid \([0-9][0-9]*\),.*/\1/p')
case "$silli_daemon_pid" in
    ''|*[!0-9]*) echo "deploy.sh: cannot identify replacement daemon: $silli_daemon_status" >&2; exit 1 ;;
esac
cmp -s "/proc/$silli_daemon_pid/exe" "$PREFIX/bin/silli" || {
    echo "deploy.sh: daemon $silli_daemon_pid is not running the installed build" >&2
    exit 1
}

# exactly one local silli-gui, the installed one, under the unit
sleep 2
windows=$(packaging/local-gui.sh)
n=$(printf '%s\n' "$windows" | grep -c . || true)
if [ "$n" -ne 1 ]; then
    echo "deploy.sh: expected one local silli-gui process, found $n:" >&2
    printf '%s\n' "$windows" >&2
    exit 1
fi
silli_gui_pid=${windows%% *}
silli_gui_exe=$(readlink "/proc/$silli_gui_pid/exe" 2>/dev/null || true)
silli_expected_exe=$(readlink -f "$PREFIX/bin/silli-gui")
case "$silli_gui_exe" in
    "$silli_expected_exe") ;;
    *) echo "deploy.sh: the running silli-gui is not $silli_expected_exe: $windows" >&2; exit 1 ;;
esac
if command -v systemctl >/dev/null 2>&1 && systemctl --user cat silli-gui.service >/dev/null 2>&1; then
    systemctl --user is-active --quiet silli-gui.service || {
        echo "deploy.sh: silli-gui.service is not active (journalctl --user -u silli-gui)" >&2
        exit 1
    }
fi
echo "deployed: $PREFIX/bin/silli $(sha256sum "$PREFIX/bin/silli" | cut -c1-12); silli-gui $silli_expected_exe running as pid $silli_gui_pid"

# what a session cannot see from its jail, into target/host-check/report.txt for the agent to read
PREFIX="$PREFIX" SILLI_BUILD_DIR="$silli_build_dir" packaging/check.sh || {
    echo "deploy.sh: deployed, but check.sh found $? failure(s): target/host-check/report.txt" >&2
    exit 1
}
