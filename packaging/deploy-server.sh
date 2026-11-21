#!/bin/sh
# Run from a clone on the Debian server, as the account that will run silli.
set -eu
cd "$(dirname "$0")/.."
usage() { echo 'usage: packaging/deploy-server.sh [--no-build | --check]'; }
silli_mode=build
case "${1:-}" in
    '') ;;
    --no-build) silli_mode=install ;;
    --check) silli_mode=check ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 1 ;;
esac
[ "$#" -le 1 ] || { usage >&2; exit 1; }
[ ! -d /.silli ] || { echo 'Run on the server host, outside a silli jail.' >&2; exit 1; }
[ "$(id -u)" -ne 0 ] || { echo 'Run as the intended silli account, not root; sudo is used for machine setup.' >&2; exit 1; }
[ -f /etc/debian_version ] && [ -d /run/systemd/system ] || { echo 'Requires Debian with systemd.' >&2; exit 1; }
SILLI_BOX=local; export SILLI_BOX
unset SILLI_NO_DAEMON_START
# Fixed system prefix matches the installed units. Serialize builds and activation.
mkdir -p target
exec 9>target/deploy-server.lock
flock -n 9 || { echo 'Another server deployment is running.' >&2; exit 1; }
if [ "$silli_mode" = check ]; then
    flock -u 9
    exec packaging/deploy-server-finish.sh --check
fi
for silli_cmd in sudo systemctl systemd-run loginctl bwrap cc pkg-config; do
    command -v "$silli_cmd" >/dev/null || {
        echo "Missing $silli_cmd. See docs/server.md for Debian prerequisites." >&2; exit 1;
    }
done
systemctl --user show-environment >/dev/null || {
    echo 'No user manager: install libpam-systemd and log in directly over SSH again.' >&2; exit 1;
}
if [ "$silli_mode" = build ]; then
    cargo build --locked --release -p silli -p silli-shim
fi
for silli_bin in silli silli-shim; do
    test -x "target/release/$silli_bin" || { echo "Missing target/release/$silli_bin" >&2; exit 1; }
done
sudo -v
# SILLI_STATE_DIR: a mounted filesystem to bind as the state filesystem instead of an image
sudo target/release/silli init --headless ${SILLI_STATE_DIR:+--state-dir "$SILLI_STATE_DIR"}
for silli_bin in silli silli-shim; do
    cmp "target/release/$silli_bin" "/usr/local/bin/$silli_bin"
done
sudo loginctl enable-linger "$(id -un)"
# An existing explicit autostart-off preference needs the operator's attention.
/usr/local/bin/silli init --user --headless
silli_unit="silli-server-deploy-$(date +%s)-$$"
echo "Activating server; logs: journalctl --user -u $silli_unit"
# Inherit the deployment lock into the service, independently of this terminal.
# The service takes its own lock after this shell releases it.
flock -u 9
exec systemd-run --user --wait --collect --unit="$silli_unit" \
    --working-directory="$PWD" -- /bin/sh "$PWD/packaging/deploy-server-finish.sh"
