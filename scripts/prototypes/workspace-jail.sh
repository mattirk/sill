#!/bin/bash
# Parallel-coding prototype, mount half: run ON THE HOST (needs unprivileged bubblewrap).
#   scripts/prototypes/workspace-jail.sh /tmp/ws-proto
# Builds a source repo inside an "ancestor pin" directory and a shared-clone workspace, then runs
# Git inside bwrap with: the ancestor pin bound read-write, the source checkout masked (tmpfs),
# only the source's object store bound back read-only at its own path, and the workspace bound.
# Nothing outside DIR is written.
set -u
D=${1:?usage: workspace-jail.sh DIR}; rm -rf "$D"; mkdir -p "$D"; D=$(cd "$D" && pwd)
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
g(){ git -c init.defaultBranch=main -c gc.auto=0 "$@"; }
P=$D/projects; S=$P/src; W=$D/ws/demo-ab12
mkdir -p "$S"; g init -q "$S"
for i in 1 2 3; do echo "line $i" > "$S/f$i.txt"; g -C "$S" add .; g -C "$S" commit -qm "c$i"; done
echo "secret source work" > "$S/uncommitted.txt"
g -C "$S" update-ref refs/sill/workspaces/ab12/base HEAD
g clone -q --shared --no-tags --single-branch -b main "$S" "$W"; g -C "$W" switch -qc sill/demo-ab12
BEFORE=$(find "$S/.git/objects" -type f | sort | sha256sum)

cat > "$D/inside.sh" <<'IN'
set -u
ok(){ echo "PASS  $*"; }; bad(){ echo "FAIL  $*"; }
check(){ if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
g(){ git -c gc.auto=0 "$@"; }
echo "   source as the jail sees it: $(cd "$S" && find . -maxdepth 2 | sort | tr '\n' ' ')"
check "source checkout hidden (tracked file)" "[ ! -e '$S/f1.txt' ]"
check "source checkout hidden (uncommitted file)" "[ ! -e '$S/uncommitted.txt' ]"
check "source refs/config/hooks hidden" "[ ! -e '$S/.git/HEAD' ] && [ ! -e '$S/.git/config' ] && [ ! -e '$S/.git/hooks' ]"
check "rest of the ancestor pin still writable" "touch '$P/other-file'"
check "borrowed object store read-only" "! touch '$S/.git/objects/x'"
check "workspace log/checkout work through the alternate" "g -C '$W' log --oneline | grep -q c1 && [ -f '$W/f1.txt' ]"
check "workspace fsck clean" "g -C '$W' fsck --full"
echo work > "$W/f2.txt"; echo n > "$W/new.txt"; g -C "$W" add .
check "commit in the workspace" "g -C '$W' commit -qm agent"
check "workspace gc --prune=now" "g -C '$W' gc -q --prune=now && g -C '$W' fsck --full"
check "workspace cannot fetch from the hidden source" "! g -C '$W' fetch -q origin"
IN

bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
  --bind "$P" "$P" \
  --tmpfs "$S" \
  --ro-bind "$S/.git/objects" "$S/.git/objects" \
  --bind "$D/ws" "$D/ws" \
  --setenv S "$S" --setenv W "$W" --setenv P "$P" \
  --setenv GIT_CONFIG_GLOBAL /dev/null --setenv GIT_CONFIG_NOSYSTEM 1 \
  -- bash -c "S='$S' W='$W' P='$P'; $(cat "$D/inside.sh")"
echo "-- back on the host"
AFTER=$(find "$S/.git/objects" -type f | sort | sha256sum)
[ "$BEFORE" = "$AFTER" ] && echo "PASS  source object store unchanged by the jailed workspace" || echo "FAIL  source object store changed"
[ -f "$S/uncommitted.txt" ] && echo "PASS  source checkout intact on the host" || echo "FAIL  source checkout"
git -C "$S" status --short | sed 's/^/   source status: /'
