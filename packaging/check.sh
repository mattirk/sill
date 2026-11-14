#!/bin/sh
# After a deploy, check on the host what a session cannot see from inside its jail (the daemon's
# socket, systemd, the journal, a jail of its own) and write it all to target/host-check/report.txt,
# where an agent in a session pinned to this checkout reads it. Run it in a host terminal
# (Ctrl+Shift+H), like deploy.sh. Nothing needs root; nothing is left behind on the host.
#
#   packaging/check.sh          # every check, the report printed and written
#   packaging/check.sh --quick  # skip the ghost session (no agent started)
#
# Exit status: the number of failed checks (0 = all passed).
set -u
cd "$(dirname "$0")/.."
if [ -d /.sill ]; then
    echo "check.sh: this is a sill jail; run it in a host terminal (Ctrl+Shift+H)" >&2
    exit 1
fi
PREFIX=${PREFIX:-/usr/local}
SILL="$PREFIX/bin/sill"
QUICK=; [ "${1:-}" = "--quick" ] && QUICK=1

OUT=target/host-check
mkdir -p "$OUT"
REPORT="$OUT/report.txt"
: >"$REPORT"
FAILED=0
TMP=$(mktemp -d)
STARTED=$(date '+%Y-%m-%d %H:%M:%S')

say() { printf '%s\n' "$*" | tee -a "$REPORT"; }
# a check's output, indented, into the report only (the terminal gets the verdict)
log() { sed 's/^/    | /' >>"$REPORT"; }
pass() { say "PASS  $*"; }
fail() { say "FAIL  $*"; FAILED=$((FAILED + 1)); }
# run NAME CMD…: the command's output into the report, PASS/FAIL on its exit status
run() {
    name=$1; shift
    if "$@" >"$TMP/out" 2>&1; then pass "$name"; else fail "$name (exit $?)"; fi
    log <"$TMP/out"
}

say "sill host check  $STARTED  $(uname -n)  HEAD $(git rev-parse --short HEAD 2>/dev/null)$(git diff --quiet 2>/dev/null || echo ' (dirty)')"
say ""

# --- installed binaries --------------------------------------------------------------------------
say "## install"
for b in sill sill-shim; do
    if [ ! -x "$PREFIX/bin/$b" ]; then
        fail "$PREFIX/bin/$b installed"
    elif [ -x "target/release/$b" ] && ! cmp -s "target/release/$b" "$PREFIX/bin/$b"; then
        fail "$PREFIX/bin/$b is target/release/$b (differs: not deployed since the last build?)"
    else
        pass "$PREFIX/bin/$b $(sha256sum "$PREFIX/bin/$b" | cut -c1-12)"
    fi
done
run "sill --version" "$SILL" --version
say ""

# --- daemon and units ----------------------------------------------------------------------------
say "## daemon"
for u in sill.socket sill.service sill-shelf.service; do
    if ! systemctl --user cat "$u" >/dev/null 2>&1; then
        say "SKIP  $u not installed"
    elif systemctl --user is-active --quiet "$u"; then
        pass "$u active"
    else
        fail "$u $(systemctl --user is-active "$u" 2>&1)"
    fi
done
run "sill daemon status" "$SILL" daemon status
# the unit the user manager runs is the one on disk, with the watchdog (docs/daemon.md §3.7):
# a rewritten /etc/systemd/user/sill.service needs a `systemctl --user daemon-reload` to land
if systemctl --user cat sill.service >/dev/null 2>&1; then
    systemctl --user show sill.service -p Type,NotifyAccess,WatchdogUSec,NRestarts,Result,ExecMainStartTimestamp,ExecMainPID,NeedDaemonReload >"$TMP/unit" 2>&1
    if grep -q '^NeedDaemonReload=no' "$TMP/unit" && grep -q '^Type=notify' "$TMP/unit" && ! grep -q '^WatchdogUSec=0$' "$TMP/unit"; then
        pass "sill.service loaded as Type=notify with a watchdog"
    else
        fail "sill.service loaded as Type=notify with a watchdog (systemctl --user daemon-reload?)"
    fi
    log <"$TMP/unit"
    # the daemon's own lines since it started: the launcher it found, READY, nothing stuck
    journalctl --user -u sill.service -n 20 --no-pager -o short 2>&1 | log
fi
run "sill session list" "$SILL" session list
run "sill changes list" "$SILL" changes list
shelves=$(pgrep -a -f 'sill shelf' || true)
n=$(printf '%s\n' "$shelves" | grep -c . || true)
if [ "$n" -eq 1 ]; then pass "one shelf process"; else fail "one shelf process (found $n)"; fi
printf '%s\n' "$shelves" | log
say ""

# --- a jail of our own: what a session sees ------------------------------------------------------
# One jail does every inside check (a second run on the same state dir is tested on its own below)
# and prints KEY=VALUE lines; its own state dir, beside the dev session's, removed at the end.
if [ -d "/var/lib/sill/$(id -u)" ]; then base="/var/lib/sill/$(id -u)/sessions"; else base="${XDG_STATE_HOME:-$HOME/.local/state}/sill/sessions"; fi
STATE="$base/check-$$"
state_home=${XDG_STATE_HOME:-$HOME/.local/state}
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
probe="$HOME/sill-check-$$.txt"
say "## jail view (state dir $STATE)"
"$SILL" jail run --state-dir "$STATE" -- sh -c '
    echo "ran=yes"
    echo "state_ls=$(ls -A "$1/sill" 2>&1 | tr "\n" " ")"
    if touch "$1/sill/tools/.check" 2>/dev/null; then echo "tools_rw=yes"; else echo "tools_rw=no"; fi
    echo "config_ls=$(ls -A "$2/sill" 2>/dev/null | tr "\n" " ")"
    echo "journal_ls=$(ls -A /var/log/journal 2>/dev/null | tr "\n" " ")"
    if SILL_NO_DAEMON_START=1 "$3" daemon status >/dev/null 2>&1; then echo "daemon=reachable"; else echo "daemon=unreachable"; fi
    if echo "sill check" >"$4"; then echo "probe=written"; else echo "probe=failed"; fi
    echo "--- id, SILL_SESSION, /.sill, mounts"
    id; echo "SILL_SESSION=${SILL_SESSION:-}"; ls -A /.sill
    findmnt -rn -o TARGET,FSTYPE,OPTIONS 2>/dev/null | head -60 || head -60 /proc/self/mounts
' check "$state_home" "$config_home" "$SILL" "$probe" >"$TMP/jail" 2>&1
jail_rc=$?
log <"$TMP/jail"
val() { sed -n "s/^$1=//p" "$TMP/jail" | head -n1; }
# check NAME CONDITION…: PASS/FAIL, or SKIP when the jail did not run
check() {
    name=$1; shift
    if [ "$(val ran)" != yes ]; then say "SKIP  $name (the jail did not run)"
    elif "$@"; then pass "$name"; else fail "$name"; fi
}
if [ "$(val ran)" = yes ]; then pass "jail run"; else fail "jail run (exit $jail_rc)"; fi
check "~/.local/state/sill shows only tools/" [ "$(val state_ls)" = "tools " ]
check "tools/ read-only in the jail" [ "$(val tools_rw)" = no ]
check "~/.config/sill empty in the jail" [ -z "$(val config_ls)" ]
check "/var/log/journal empty in the jail" [ -z "$(val journal_ls)" ]
check "no daemon reachable from the jail" [ "$(val daemon)" = unreachable ]
say ""

# --- changes by hand: list, keep, undo the probe the jail wrote ----------------------------------
say "## changes (sill jail changes/keep/unkeep on that state dir)"
if [ "$(val probe)" != written ]; then
    say "SKIP  changes by hand (the jail did not write $probe)"
else
    if [ -e "$probe" ]; then fail "the write stayed in the overlay (it is on the host)"; else pass "the write stayed in the overlay"; fi
    "$SILL" jail changes --state-dir "$STATE" >"$TMP/out" 2>&1
    if grep -qF "$probe" "$TMP/out"; then pass "jail changes lists it"; else fail "jail changes lists it"; fi
    log <"$TMP/out"
    run "jail keep" "$SILL" jail keep --state-dir "$STATE" "$probe"
    if [ "$(cat "$probe" 2>/dev/null)" = "sill check" ]; then
        pass "kept to the host"
        run "jail unkeep" "$SILL" jail unkeep --state-dir "$STATE" "$probe"
        if [ -e "$probe" ]; then fail "unkeep took it off the host"; rm -f "$probe"; else pass "unkeep took it off the host"; fi
    else
        fail "kept to the host"
    fi
fi
say ""

# --- the same state dir again: what reopening a session does -------------------------------------
# Right after a jail ends its overlay is still being torn down and a new mount on the same upper
# tree gets EBUSY for a moment; `sill launch` retries for that (main.rs, "jail ended at once"),
# `sill jail run` execs bwrap and cannot — so at once is only noted, 2 s later must work.
say "## jail again on the same state dir"
again() { "$SILL" jail run --state-dir "$STATE" -- true >"$TMP/out" 2>&1; }
if again; then pass "second jail run, at once"; else say "NOTE  second jail run, at once, refused (overlay teardown; sessions retry)"; fi
log <"$TMP/out"
sleep 2
if again; then pass "third jail run, 2 s later"; else fail "third jail run, 2 s later (exit $?)"; fi
log <"$TMP/out"
say ""

# --- a session through the daemon ----------------------------------------------------------------
if [ -z "$QUICK" ]; then
    say "## ghost session through the daemon"
    if "$SILL" session new --ghost >"$TMP/id" 2>"$TMP/err"; then
        id=$(head -n1 "$TMP/id")
        pass "session new --ghost: $id"
        cat "$TMP/id" "$TMP/err" | log
        sleep 2
        "$SILL" session list >"$TMP/out" 2>&1
        if grep -q "^$id" "$TMP/out"; then pass "session list shows it"; else fail "session list shows it"; fi
        log <"$TMP/out"
        run "daemon status with it" "$SILL" daemon status
        run "changes list with it" "$SILL" changes list
        run "session end" "$SILL" session end "$id"
        sleep 2
        "$SILL" session list >"$TMP/out" 2>&1
        if grep -q "^$id" "$TMP/out"; then fail "the ghost is gone after end"; else pass "the ghost is gone after end"; fi
        log <"$TMP/out"
    else
        fail "session new --ghost (exit $?)"
        cat "$TMP/id" "$TMP/err" | log
    fi
    say ""
fi

# --- logs a session cannot read ------------------------------------------------------------------
say "## journal since $STARTED (sill units and sessions, last 80 lines)"
journalctl --user --since "$STARTED" -o short-monotonic --no-pager \
    -u sill.service -u sill.socket -u sill-shelf.service -t sill-dev 2>&1 | tail -n 80 | log
journalctl --user --since "-1h" -p warning --no-pager -u sill.service -u sill-shelf.service 2>&1 | tail -n 40 >"$TMP/out"
if grep -q . "$TMP/out" && ! grep -q '^-- No entries --$' "$TMP/out"; then
    say "NOTE  warnings or worse from sill in the last hour:"
    log <"$TMP/out"
fi
say ""

rm -rf "$STATE" "$TMP" 2>/dev/null
say "$FAILED failed — report in $REPORT"
exit "$FAILED"
