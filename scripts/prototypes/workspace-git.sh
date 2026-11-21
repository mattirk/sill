#!/bin/bash
# Parallel-coding prototype, Git half: shared-clone workspace mechanics without a jail.
# Usage: workspace-git.sh DIR   (DIR is created fresh; nothing outside it is touched)
set -u
D=$1; rm -rf "$D"; mkdir -p "$D"; cd "$D"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
ok(){ echo "PASS  $*"; }; bad(){ echo "FAIL  $*"; }
check(){ if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
g(){ git -c init.defaultBranch=main -c gc.auto=0 "$@"; }

# --- source with history
S=$D/projects/src; mkdir -p "$S"; g init -q "$S"
for i in 1 2 3; do echo "line $i" > "$S/f$i.txt"; g -C "$S" add .; g -C "$S" commit -qm "c$i"; done
BASE=$(g -C "$S" rev-parse HEAD)

echo "== 1. clone options"
for mode in shared reference; do
  W=$D/ws-$mode
  if [ $mode = shared ]; then g clone -q --shared --no-tags --single-branch -b main "$S" "$W"
  else g clone -q --no-local --reference "$S" --no-tags --single-branch -b main "$S" "$W"; fi
  echo "   $mode: alternates=$(cat $W/.git/objects/info/alternates) own-objects=$(g -C $W count-objects -v | awk '/^count|^in-pack/{s+=$2} END{print s}')"
done
rm -rf "$D/ws-reference"

# --- the workspace proper: shared clone, branch at base, retention anchor first
ID=ab12; W=$D/ws/demo-$ID
g -C "$S" update-ref refs/silli/workspaces/$ID/base "$BASE"
g clone -q --shared --no-tags --single-branch -b main "$S" "$W"
g -C "$W" switch -q -c silli/demo-$ID
echo "agent work" > "$W/f2.txt"; echo new > "$W/new.txt"; g -C "$W" add .; g -C "$W" commit -qm "agent change"
WTIP=$(g -C "$W" rev-parse HEAD)
check "workspace commit exists only in the clone's own store" "! g -C '$S' cat-file -e $WTIP && g -C '$W' cat-file -e $WTIP"

echo "== 2. retention under hostile source GC"
# control clone with NO anchor, on a source branch nothing retains
g -C "$S" switch -qc side; echo side > "$S/side.txt"; g -C "$S" add side.txt; g -C "$S" commit -qm side; g -C "$S" switch -q main
C=$D/ws/control; g clone -q --shared --no-tags --single-branch -b side "$S" "$C"
g -C "$S" branch -qD side
# rewrite source history: orphan main, drop everything else, expire reflogs, prune
g -C "$S" checkout -q --orphan tmp; g -C "$S" rm -qrf .; echo fresh > "$S/fresh.txt"; g -C "$S" add .; g -C "$S" commit -qm fresh
g -C "$S" branch -qD main; g -C "$S" branch -qm main
g -C "$S" reflog expire --expire=now --all; g -C "$S" gc -q --prune=now
check "anchored workspace: fsck clean after source gc --prune=now" "g -C '$W' fsck --full"
check "anchored workspace: base files readable" "g -C '$W' show $BASE:f1.txt"
check "control (no anchor) is broken, proving the anchor matters" "! g -C '$C' fsck --full"
rm -rf "$C"
# restore a normal source for the remaining steps: main back at BASE
g -C "$S" checkout -q -f -B main "$BASE"; g -C "$S" clean -qfdx

echo "== 3. rebase onto a moved source without fetching"
echo "host work" > "$S/f3.txt"; g -C "$S" commit -qam "host change"; DEST=$(g -C "$S" rev-parse HEAD)
g -C "$S" update-ref refs/silli/workspaces/$ID/dest "$DEST"        # retain before the workspace uses it
g -C "$W" update-ref refs/silli/dest "$DEST"                        # objects already reachable via alternates
check "workspace sees destination tip without a fetch" "g -C '$W' cat-file -e $DEST"
g -C "$W" update-ref refs/silli/pre-rebase "$WTIP"
check "rebase onto destination succeeds in the workspace" "g -C '$W' rebase -q refs/silli/dest"
NEWTIP=$(g -C "$W" rev-parse HEAD)

echo "== 4. host-side Git and workspace-controlled config"
CAN=$D/canary.log; cat > "$D/canary.sh" <<'C'
#!/bin/sh
echo "ran: $0 $*" >> "${CANARY:?}"
C
chmod +x "$D/canary.sh"; export CANARY=$CAN
mk(){ printf '#!/bin/sh\necho %s >> "$CANARY"\n' "$1" > "$D/c-$1"; chmod +x "$D/c-$1"; echo "$D/c-$1"; }
g -C "$W" config core.fsmonitor "$(mk fsmonitor)"
g -C "$W" config core.hooksPath "$D/hooks"; mkdir -p "$D/hooks"; for h in pre-push post-checkout reference-transaction; do cp "$(mk hook-$h)" "$D/hooks/$h"; done
g -C "$W" config uploadpack.packObjectsHook "$(mk packObjectsHook)"
g -C "$W" config filter.x.smudge "$(mk smudge)"; g -C "$W" config filter.x.clean "$(mk clean)"; echo "* filter=x" > "$W/.git/info/attributes"
g -C "$W" config core.pager "$(mk pager)"; g -C "$W" config diff.external "$(mk diff.external)"
g -C "$S" fetch -q "$W" silli/demo-$ID:refs/silli/workspaces/$ID/incoming 2>/dev/null
echo "   host fetch from the workspace ran: $(cat $CAN 2>/dev/null | sort -u | tr '\n' ';' || true)"
check "host fetch from the workspace ran no workspace-configured program" "[ ! -s '$CAN' ]"
: > "$CAN"; g -C "$W" status --short >/dev/null 2>&1; g -C "$W" diff >/dev/null 2>&1
echo "   plain host 'git status/diff' inside the workspace ran: $(sort -u $CAN | tr '\n' ';')"
: > "$CAN"; g -c core.fsmonitor=false -c core.hooksPath=/dev/null -C "$W" status --short >/dev/null 2>&1
echo "   status with fsmonitor/hooks overridden ran: $(sort -u $CAN | tr '\n' ';')"
check "fetched result's new objects are in the source's own store" "g -C '$S' cat-file -e $NEWTIP && ! grep -q ws '$S/.git/objects/info/alternates' 2>/dev/null"

echo "== 5. fast-forward merge preserving source edits"
echo "my unrelated edit" >> "$S/f1.txt"; echo "staged" > "$S/staged.txt"; g -C "$S" add staged.txt
check "ff merge with unrelated dirty + staged files succeeds" "g -C '$S' merge -q --ff-only refs/silli/workspaces/$ID/incoming"
check "unrelated edit survived" "grep -q 'my unrelated edit' '$S/f1.txt'"
check "staged file still staged" "g -C '$S' diff --cached --name-only | grep -q staged.txt"
check "branch, index and files updated" "[ \"\$(g -C '$S' rev-parse main)\" = $NEWTIP ] && grep -q 'agent work' '$S/f2.txt' && g -C '$S' ls-files | grep -q new.txt"
# overlap: a second workspace changes f1.txt while the source has f1.txt dirty
g -C "$S" stash -q; g -C "$S" commit -qm noop --allow-empty 2>/dev/null; g -C "$S" stash pop -q
W2=$D/ws/two; g clone -q --shared --no-tags --single-branch -b main "$S" "$W2"; g -C "$W2" switch -qc b2
echo "agent f1" > "$W2/f1.txt"; g -C "$W2" commit -qam "touch f1"
g -C "$S" fetch -q "$W2" b2:refs/silli/workspaces/two/incoming
check "overlapping dirty file stops the ff merge" "! g -C '$S' merge -q --ff-only refs/silli/workspaces/two/incoming"
check "...and the person's edit is untouched" "grep -q 'my unrelated edit' '$S/f1.txt'"
echo "untracked" > "$S/u.txt"; g -C "$W2" checkout -q b2; echo x > "$W2/u.txt"; g -C "$W2" add u.txt; g -C "$W2" commit -qm "adds u"
g -C "$S" fetch -q "$W2" b2:refs/silli/workspaces/two/incoming
check "untracked-file collision stops the ff merge" "! g -C '$S' merge -q --ff-only refs/silli/workspaces/two/incoming && grep -q untracked '$S/u.txt'"

echo "== 6. after merge, remove clones and anchors"
rm -rf "$D/ws" "$D/ws-shared"; for r in $(g -C "$S" for-each-ref --format='%(refname)' refs/silli/); do g -C "$S" update-ref -d "$r"; done
g -C "$S" reflog expire --expire=now --all; g -C "$S" gc -q --prune=now
check "source fsck clean without any workspace" "g -C '$S' fsck --full"
check "fresh clone of the source checks out" "g clone -q '$S' '$D/fresh' && [ -f '$D/fresh/new.txt' ]"
