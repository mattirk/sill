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

# Keep restart AND verification outside the daemon/GUI/host-shell process groups.
# The service continues even if restarting sill closes the terminal that launched deployment.
sill_deploy_unit="sill-deploy-$(date +%s)-$$"
echo "Restarting daemon, GUI and running jails; log: journalctl --user -u $sill_deploy_unit"
exec systemd-run --user --wait --collect --unit="$sill_deploy_unit" \
    --working-directory="$PWD" -- /bin/sh "$PWD/packaging/deploy-restart.sh" \
    "$PREFIX" "$sill_build_dir"
