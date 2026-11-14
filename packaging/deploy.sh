#!/bin/sh
# Build, install and restart sill on this machine, then check that the shelf on screen is the build
# just made. Run it in a host terminal (Ctrl+Shift+H) — not in a session's shell: sudo is refused
# inside the jail by design (SPEC §4.2).
#
#   packaging/deploy.sh             # cargo build --release, sudo sill init, sill restart, verify,
#                                   # then packaging/check.sh (report in target/host-check/)
#   packaging/deploy.sh --fast      # faster incremental build in target/local, then deploy
#   packaging/deploy.sh --no-build  # install and restart what target/release already has
#   packaging/deploy.sh --ui=both   # which desktop frontends to install (default: both):
#       both  sill with the GTK shelf, and sill-gui beside it; the frontend chosen stays as it is
#       gtk   sill with the GTK shelf; the frontend becomes gtk (sill-gui, if installed, stays)
#       iced  sill without GTK, and sill-gui; the frontend becomes iced
#     The two stay separate binaries (docs/sill-gui.md, packaging); `sill frontend gtk|iced` and
#     `sill restart` switch between installed ones without a rebuild.
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
sill_ui=both
for arg in "$@"; do
    case "$arg" in
        --fast) sill_profile=local ;;
        --no-build) sill_build=no ;;
        --ui=both|--ui=gtk|--ui=iced) sill_ui=${arg#--ui=} ;;
        *) echo "usage: packaging/deploy.sh [--fast] [--no-build] [--ui=both|gtk|iced]" >&2; exit 1 ;;
    esac
done
sill_build_dir="target/$sill_profile"
sudo -v # the password now, not after the build
if [ "$sill_build" = yes ]; then
    case "$sill_ui" in
        # sill without the GTK shelf: no GTK in the daemon's binary at all
        iced)
            cargo build --profile "$sill_profile" -p sill-shelf --no-default-features
            cargo build --profile "$sill_profile" -p sill-gui
            ;;
        # the workspace: sill with the GTK shelf, sill-shim and sill-gui
        *) cargo build --profile "$sill_profile" ;;
    esac
fi
sill_bins="sill sill-shim"
[ "$sill_ui" = gtk ] || sill_bins="$sill_bins sill-gui"
for b in $sill_bins; do
    [ -x "$sill_build_dir/$b" ] || { echo "deploy.sh: $sill_build_dir/$b missing" >&2; exit 1; }
done

sudo "$sill_build_dir/sill" init --prefix "$PREFIX"
# what init installed must be what was built
for b in sill sill-shim; do
    cmp -s "$sill_build_dir/$b" "$PREFIX/bin/$b" || {
        echo "deploy.sh: $PREFIX/bin/$b differs from $sill_build_dir/$b after init" >&2
        exit 1
    }
done
# sill-gui is its own binary: installed beside sill, not by init
if [ "$sill_ui" != gtk ]; then
    sudo install -m 0755 "$sill_build_dir/sill-gui" "$PREFIX/bin/sill-gui"
    cmp -s "$sill_build_dir/sill-gui" "$PREFIX/bin/sill-gui" || {
        echo "deploy.sh: $PREFIX/bin/sill-gui differs from $sill_build_dir/sill-gui" >&2
        exit 1
    }
fi
# the frontend `sill shelf` runs (as this user, in ~/.config/sill/sill.toml)
case "$sill_ui" in
    gtk) "$PREFIX/bin/sill" frontend gtk ;;
    iced) "$PREFIX/bin/sill" frontend iced ;;
esac
sill_frontend=$("$PREFIX/bin/sill" frontend | head -n 1)

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
case "$sill_frontend" in
    iced) sill_expected_exe=$(readlink -f "$PREFIX/bin/sill-gui") ;;
    *) sill_expected_exe=$(readlink -f "$PREFIX/bin/sill") ;;
esac
case "$sill_shelf_exe" in
    "$sill_expected_exe") ;;
    *) echo "deploy.sh: the running local frontend ($sill_frontend) is not $sill_expected_exe: $shelves" >&2; exit 1 ;;
esac
if command -v systemctl >/dev/null 2>&1 && systemctl --user cat sill-shelf.service >/dev/null 2>&1; then
    systemctl --user is-active --quiet sill-shelf.service || {
        echo "deploy.sh: sill-shelf.service is not active (journalctl --user -u sill-shelf)" >&2
        exit 1
    }
fi
echo "deployed: $PREFIX/bin/sill $(sha256sum "$PREFIX/bin/sill" | cut -c1-12); frontend $sill_frontend: $sill_expected_exe running as pid ${shelves%% *}"

# what a session cannot see from its jail, into target/host-check/report.txt for the agent to read
PREFIX="$PREFIX" SILL_BUILD_DIR="$sill_build_dir" packaging/check.sh || {
    echo "deploy.sh: deployed, but check.sh found $? failure(s): target/host-check/report.txt" >&2
    exit 1
}
