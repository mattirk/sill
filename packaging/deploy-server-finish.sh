#!/bin/sh
# Internal activation/check phase. Does not invoke a GUI or stop jails.
set -eu
cd "$(dirname "$0")/.."
SILLI_BOX=local; export SILLI_BOX
unset SILLI_NO_DAEMON_START
exec 9>target/deploy-server.lock
flock -n 9 || { echo 'Another deployment is running; activation refused.' >&2; exit 1; }
silli=/usr/local/bin/silli
if [ "${1:-}" != --check ]; then
    systemctl --user daemon-reload
    # Honor explicit autostart-off rather than reporting a boot-persistent install.
    test "$("$silli" daemon autostart)" = 'daemon autostart: on' || {
        echo 'Autostart is disabled. Run silli daemon autostart on, then rerun deployment.' >&2; exit 1;
    }
    "$silli" daemon restart
fi
systemctl --user is-enabled --quiet silli.socket
systemctl --user is-active --quiet silli.socket
systemctl --user is-active --quiet silli.service
mountpoint -q /var/lib/silli
test "$(loginctl show-user "$(id -un)" -p Linger --value)" = yes
silli_status=$("$silli" daemon status)
printf '%s\n' "$silli_status"
silli_pid=$(printf '%s\n' "$silli_status" | sed -n 's/.*pid \([0-9][0-9]*\),.*/\1/p')
case "$silli_pid" in ''|*[!0-9]*) echo 'Cannot identify daemon PID.' >&2; exit 1 ;; esac
cmp -s "/proc/$silli_pid/exe" "$silli" || { echo 'Daemon is not running the installed build.' >&2; exit 1; }
echo 'Server daemon verified. Logs: journalctl --user -u silli.service'
