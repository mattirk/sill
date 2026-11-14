#!/bin/sh
# Run from a clone on the Debian server, as the account that will run sill.
set -eu
cd "$(dirname "$0")/.."
usage() { echo 'usage: packaging/deploy-server.sh [--no-build | --check]'; }
sill_mode=build
case "${1:-}" in
    '') ;;
    --no-build) sill_mode=install ;;
    --check) sill_mode=check ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 1 ;;
esac
[ "$#" -le 1 ] || { usage >&2; exit 1; }
[ ! -d /.sill ] || { echo 'Run on the server host, outside a sill jail.' >&2; exit 1; }
[ "$(id -u)" -ne 0 ] || { echo 'Run as the intended sill account, not root; sudo is used for machine setup.' >&2; exit 1; }
[ -f /etc/debian_version ] && [ -d /run/systemd/system ] || { echo 'Requires Debian with systemd.' >&2; exit 1; }
SILL_BOX=local; export SILL_BOX
unset SILL_NO_DAEMON_START
# Fixed system prefix matches the installed units. Serialize builds and activation.
mkdir -p target
exec 9>target/deploy-server.lock
flock -n 9 || { echo 'Another server deployment is running.' >&2; exit 1; }
if [ "$sill_mode" = check ]; then
    flock -u 9
    exec packaging/deploy-server-finish.sh --check
fi
for sill_cmd in sudo systemctl systemd-run loginctl bwrap cc pkg-config; do
    command -v "$sill_cmd" >/dev/null || {
        echo "Missing $sill_cmd. See docs/server.md for Debian prerequisites." >&2; exit 1;
    }
done
systemctl --user show-environment >/dev/null || {
    echo 'No user manager: install libpam-systemd and log in directly over SSH again.' >&2; exit 1;
}
if [ "$sill_mode" = build ]; then
    cargo build --locked --release -p sill -p sill-shim
fi
for sill_bin in sill sill-shim; do
    test -x "target/release/$sill_bin" || { echo "Missing target/release/$sill_bin" >&2; exit 1; }
done
sudo -v
sudo target/release/sill init --headless
for sill_bin in sill sill-shim; do
    cmp "target/release/$sill_bin" "/usr/local/bin/$sill_bin"
done
sudo loginctl enable-linger "$(id -un)"
# An existing explicit autostart-off preference needs the operator's attention.
/usr/local/bin/sill init --user --headless
sill_unit="sill-server-deploy-$(date +%s)-$$"
echo "Activating server; logs: journalctl --user -u $sill_unit"
# Inherit the deployment lock into the service, independently of this terminal.
# The service takes its own lock after this shell releases it.
flock -u 9
exec systemd-run --user --wait --collect --unit="$sill_unit" \
    --working-directory="$PWD" -- /bin/sh "$PWD/packaging/deploy-server-finish.sh"
