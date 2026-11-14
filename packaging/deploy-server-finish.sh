#!/bin/sh
# Internal activation/check phase. Does not invoke a GUI or stop jails.
set -eu
cd "$(dirname "$0")/.."
SILL_BOX=local; export SILL_BOX
unset SILL_NO_DAEMON_START
exec 9>target/deploy-server.lock
flock -n 9 || { echo 'Another deployment is running; activation refused.' >&2; exit 1; }
sill=/usr/local/bin/sill
if [ "${1:-}" != --check ]; then
    systemctl --user daemon-reload
    # Honor explicit autostart-off rather than reporting a boot-persistent install.
    test "$("$sill" daemon autostart)" = 'daemon autostart: on' || {
        echo 'Autostart is disabled. Run sill daemon autostart on, then rerun deployment.' >&2; exit 1;
    }
    "$sill" daemon restart
fi
systemctl --user is-enabled --quiet sill.socket
systemctl --user is-active --quiet sill.socket
systemctl --user is-active --quiet sill.service
mountpoint -q /var/lib/sill
test "$(loginctl show-user "$(id -un)" -p Linger --value)" = yes
sill_status=$("$sill" daemon status)
printf '%s\n' "$sill_status"
sill_pid=$(printf '%s\n' "$sill_status" | sed -n 's/.*pid \([0-9][0-9]*\),.*/\1/p')
case "$sill_pid" in ''|*[!0-9]*) echo 'Cannot identify daemon PID.' >&2; exit 1 ;; esac
cmp -s "/proc/$sill_pid/exe" "$sill" || { echo 'Daemon is not running the installed build.' >&2; exit 1; }
echo 'Server daemon verified. Logs: journalctl --user -u sill.service'
