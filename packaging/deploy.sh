#!/bin/sh
# Build, install and restart silli on this machine, then check that the window on screen is the build
# just made. Run it in a host terminal (Ctrl+Shift+H) — not in a session's shell: sudo is refused
# inside the jail by design (SPEC §4.2).
#
#   packaging/deploy.sh             # cargo build --release, sudo silli init, silli restart, verify,
#                                   # then packaging/check.sh (report in target/host-check/)
#   packaging/deploy.sh --fast      # faster incremental build in target/local, then deploy
#   packaging/deploy.sh --no-build  # install and restart what target/release already has
set -eu
cd "$(dirname "$0")/.."
if [ -d /.silli ]; then
    echo "deploy.sh: this is a silli jail; run it in a host terminal (Ctrl+Shift+H)" >&2
    exit 1
fi
# Deployment and host checks always address this machine, even from a shell with SILLI_BOX set.
SILLI_BOX=local; export SILLI_BOX
PREFIX=${PREFIX:-/usr/local}
silli_profile=release
silli_build=yes
for arg in "$@"; do
    case "$arg" in
        --fast) silli_profile=local ;;
        --no-build) silli_build=no ;;
        *) echo "usage: packaging/deploy.sh [--fast] [--no-build]" >&2; exit 1 ;;
    esac
done
silli_build_dir="target/$silli_profile"
sudo -v # the password now, not after the build
if [ "$silli_build" = yes ]; then
    # the workspace: silli, silli-shim and silli-gui
    cargo build --profile "$silli_profile"
fi
for b in silli silli-shim silli-gui; do
    [ -x "$silli_build_dir/$b" ] || { echo "deploy.sh: $silli_build_dir/$b missing" >&2; exit 1; }
done

# init installs the three binaries from next to the one that runs it
sudo "$silli_build_dir/silli" init --prefix "$PREFIX"
# what init installed must be what was built
for b in silli silli-shim silli-gui; do
    cmp -s "$silli_build_dir/$b" "$PREFIX/bin/$b" || {
        echo "deploy.sh: $PREFIX/bin/$b differs from $silli_build_dir/$b after init" >&2
        exit 1
    }
done

# Keep restart AND verification outside the daemon/GUI/host-shell process groups.
# The service continues even if restarting silli closes the terminal that launched deployment.
silli_deploy_unit="silli-deploy-$(date +%s)-$$"
echo "Restarting daemon, GUI and running jails; log: journalctl --user -u $silli_deploy_unit"
exec systemd-run --user --wait --collect --unit="$silli_deploy_unit" \
    --working-directory="$PWD" -- /bin/sh "$PWD/packaging/deploy-restart.sh" \
    "$PREFIX" "$silli_build_dir"
