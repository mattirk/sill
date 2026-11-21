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
if [ -d /.silli ]; then
    echo "check.sh: this is a silli jail; run it in a host terminal (Ctrl+Shift+H)" >&2
    exit 1
fi
# Deployment and host checks always address this machine, even from a shell with SILLI_BOX set.
SILLI_BOX=local; export SILLI_BOX
PREFIX=${PREFIX:-/usr/local}
SILLI="$PREFIX/bin/silli"
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

say "silli host check  $STARTED  $(uname -n)  HEAD $(git rev-parse --short HEAD 2>/dev/null)$(git diff --quiet 2>/dev/null || echo ' (dirty)')"
say ""

# --- installed binaries --------------------------------------------------------------------------
say "## install"
# deploy.sh names the build it installed (SILLI_BUILD_DIR: target/release, or target/local with
# --fast); run on its own, the installed binary may be either profile's
silli_build_dirs=${SILLI_BUILD_DIR:-target/release target/local}
for b in silli silli-shim silli-gui; do
    if [ ! -x "$PREFIX/bin/$b" ]; then
        fail "$PREFIX/bin/$b installed"
        continue
    fi
    silli_built="" silli_match=""
    for d in $silli_build_dirs; do
        [ -x "$d/$b" ] || continue
        silli_built="$silli_built $d/$b"
        if cmp -s "$d/$b" "$PREFIX/bin/$b"; then silli_match="$d/$b"; break; fi
    done
    if [ -n "$silli_built" ] && [ -z "$silli_match" ]; then
        fail "$PREFIX/bin/$b is none of$silli_built (differs: not deployed since the last build?)"
    else
        pass "$PREFIX/bin/$b $(sha256sum "$PREFIX/bin/$b" | cut -c1-12)${silli_match:+ = $silli_match}"
    fi
done
run "silli --version" "$SILLI" --version
say ""

# --- daemon and units ----------------------------------------------------------------------------
say "## daemon"
for u in silli.socket silli.service silli-gui.service; do
    if ! systemctl --user cat "$u" >/dev/null 2>&1; then
        say "SKIP  $u not installed"
    elif systemctl --user is-active --quiet "$u"; then
        pass "$u active"
    else
        fail "$u $(systemctl --user is-active "$u" 2>&1)"
    fi
done
run "silli daemon status" "$SILLI" daemon status
# the unit the user manager runs is the one on disk, with the watchdog (docs/daemon.md §3.7):
# a rewritten /etc/systemd/user/silli.service needs a `systemctl --user daemon-reload` to land
if systemctl --user cat silli.service >/dev/null 2>&1; then
    systemctl --user show silli.service -p Type,NotifyAccess,WatchdogUSec,NRestarts,Result,ExecMainStartTimestamp,ExecMainPID,NeedDaemonReload >"$TMP/unit" 2>&1
    if grep -q '^NeedDaemonReload=no' "$TMP/unit" && grep -q '^Type=notify' "$TMP/unit" && ! grep -q '^WatchdogUSec=0$' "$TMP/unit"; then
        pass "silli.service loaded as Type=notify with a watchdog"
    else
        fail "silli.service loaded as Type=notify with a watchdog (systemctl --user daemon-reload?)"
    fi
    log <"$TMP/unit"
    # the daemon's own lines since it started: the launcher it found, READY, nothing stuck
    journalctl --user -u silli.service -n 20 --no-pager -o short 2>&1 | log
fi
run "silli session list" "$SILLI" session list
# nothing outlives its session (docs/daemon.md §3.11): a jail or relay of a session that is not
# open is a stray the daemon stops within two minutes, and each such stop is in its journal — a
# bug somewhere else, worth reading
running=$("$SILLI" session list 2>/dev/null | awk '$2 == "running" {print $1}')
strays=$({
    systemctl --user list-units 'silli-*.scope' --no-legend --plain 2>/dev/null | awk '{print $1}' | sed 's/^silli-//; s/\.scope$//'
    pgrep -af '[s]ill daemon relay --session' 2>/dev/null | sed -n 's/.* --session \([0-9a-f]\{8,\}\).*/\1/p'
} | sort -u | while read -r id; do
    printf '%s\n' "$running" | grep -qx "$id" || printf '%s ' "$id"
done)
if [ -z "$strays" ]; then pass "no jail or relay of a session that is not open"; else fail "jails or relays of sessions that are not open: $strays"; fi
if systemctl --user cat silli.service >/dev/null 2>&1; then
    since=$(systemctl --user show silli.service -p ExecMainStartTimestamp --value)
    journalctl --user -u silli.service --since "$since" --no-pager -o cat 2>/dev/null | grep 'still running: stopping it' >"$TMP/reaped" || true
    n=$(grep -c . "$TMP/reaped" || true)
    if [ "$n" -gt 0 ]; then say "WARN  the daemon stopped $n stray(s) since it started — each is a bug elsewhere"; log <"$TMP/reaped"; fi
fi
run "silli changes list" "$SILLI" changes list
windows=$(packaging/local-gui.sh)
n=$(printf '%s\n' "$windows" | grep -c . || true)
if [ "$n" -eq 1 ]; then pass "one local silli-gui process"; else fail "one local silli-gui process (found $n)"; fi
printf '%s\n' "$windows" | log
say ""

# --- a jail of our own: what a session sees ------------------------------------------------------
# One jail does every inside check (a second run on the same state dir is tested on its own below)
# and prints KEY=VALUE lines; its own state dir, beside the dev session's, removed at the end.
if [ -d "/var/lib/silli/$(id -u)" ]; then base="/var/lib/silli/$(id -u)/sessions"; else base="${XDG_STATE_HOME:-$HOME/.local/state}/silli/sessions"; fi
STATE="$base/check-$$"
state_home=${XDG_STATE_HOME:-$HOME/.local/state}
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
probe="$HOME/silli-check-$$.txt"
say "## jail view (state dir $STATE)"
"$SILLI" jail run --state-dir "$STATE" -- sh -c '
    echo "ran=yes"
    echo "state_ls=$(ls -A "$1/silli" 2>&1 | tr "\n" " ")"
    if touch "$1/silli/tools/.check" 2>/dev/null; then echo "tools_rw=yes"; else echo "tools_rw=no"; fi
    echo "config_ls=$(ls -A "$2/silli" 2>/dev/null | tr "\n" " ")"
    echo "journal_ls=$(ls -A /var/log/journal 2>/dev/null | tr "\n" " ")"
    if SILLI_NO_DAEMON_START=1 "$3" daemon status >/dev/null 2>&1; then echo "daemon=reachable"; else echo "daemon=unreachable"; fi
    if echo "silli check" >"$4"; then echo "probe=written"; else echo "probe=failed"; fi
    echo "--- id, SILLI_SESSION, /.silli, mounts"
    id; echo "SILLI_SESSION=${SILLI_SESSION:-}"; ls -A /.silli
    findmnt -rn -o TARGET,FSTYPE,OPTIONS 2>/dev/null | head -60 || head -60 /proc/self/mounts
' check "$state_home" "$config_home" "$SILLI" "$probe" >"$TMP/jail" 2>&1
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
check "~/.local/state/silli shows only tools/" [ "$(val state_ls)" = "tools " ]
check "tools/ read-only in the jail" [ "$(val tools_rw)" = no ]
check "~/.config/silli empty in the jail" [ -z "$(val config_ls)" ]
check "/var/log/journal empty in the jail" [ -z "$(val journal_ls)" ]
check "no daemon reachable from the jail" [ "$(val daemon)" = unreachable ]
say ""

# --- changes by hand: list, keep, undo the probe the jail wrote ----------------------------------
say "## changes (silli jail changes/keep/unkeep on that state dir)"
if [ "$(val probe)" != written ]; then
    say "SKIP  changes by hand (the jail did not write $probe)"
else
    if [ -e "$probe" ]; then fail "the write stayed in the overlay (it is on the host)"; else pass "the write stayed in the overlay"; fi
    "$SILLI" jail changes --state-dir "$STATE" >"$TMP/out" 2>&1
    if grep -qF "$probe" "$TMP/out"; then pass "jail changes lists it"; else fail "jail changes lists it"; fi
    log <"$TMP/out"
    run "jail keep" "$SILLI" jail keep --state-dir "$STATE" "$probe"
    if [ "$(cat "$probe" 2>/dev/null)" = "silli check" ]; then
        pass "kept to the host"
        run "jail unkeep" "$SILLI" jail unkeep --state-dir "$STATE" "$probe"
        if [ -e "$probe" ]; then fail "unkeep took it off the host"; rm -f "$probe"; else pass "unkeep took it off the host"; fi
    else
        fail "kept to the host"
    fi
fi
say ""

# --- the same state dir again: what reopening a session does -------------------------------------
# Right after a jail ends its overlay is still being torn down and a new mount on the same upper
# tree gets EBUSY for a moment; `silli launch` retries for that (main.rs, "jail ended at once"),
# `silli jail run` execs bwrap and cannot — so at once is only noted, 2 s later must work.
say "## jail again on the same state dir"
again() { "$SILLI" jail run --state-dir "$STATE" -- true >"$TMP/out" 2>&1; }
if again; then pass "second jail run, at once"; else say "NOTE  second jail run, at once, refused (overlay teardown; sessions retry)"; fi
log <"$TMP/out"
sleep 2
if again; then pass "third jail run, 2 s later"; else fail "third jail run, 2 s later (exit $?)"; fi
log <"$TMP/out"
say ""

# --- a session through the daemon ----------------------------------------------------------------
if [ -z "$QUICK" ]; then
    say "## ghost session through the daemon"
    if "$SILLI" session new --ghost >"$TMP/id" 2>"$TMP/err"; then
        id=$(head -n1 "$TMP/id")
        pass "session new --ghost: $id"
        cat "$TMP/id" "$TMP/err" | log
        sleep 2
        "$SILLI" session list >"$TMP/out" 2>&1
        if grep -q "^$id" "$TMP/out"; then pass "session list shows it"; else fail "session list shows it"; fi
        log <"$TMP/out"
        run "daemon status with it" "$SILLI" daemon status
        run "changes list with it" "$SILLI" changes list
        run "session end" "$SILLI" session end "$id"
        sleep 2
        "$SILLI" session list >"$TMP/out" 2>&1
        if grep -q "^$id" "$TMP/out"; then fail "the ghost is gone after end"; else pass "the ghost is gone after end"; fi
        log <"$TMP/out"
    else
        fail "session new --ghost (exit $?)"
        cat "$TMP/id" "$TMP/err" | log
    fi
    say ""
fi

# --- logs a session cannot read ------------------------------------------------------------------
say "## journal since $STARTED (silli units and sessions, last 80 lines)"
journalctl --user --since "$STARTED" -o short-monotonic --no-pager \
    -u silli.service -u silli.socket -u silli-gui.service -t silli-dev 2>&1 | tail -n 80 | log
journalctl --user --since "-1h" -p warning --no-pager -u silli.service -u silli-gui.service 2>&1 | tail -n 40 >"$TMP/out"
if grep -q . "$TMP/out" && ! grep -q '^-- No entries --$' "$TMP/out"; then
    say "NOTE  warnings or worse from silli in the last hour:"
    log <"$TMP/out"
fi
say ""

rm -rf "$STATE" "$TMP" 2>/dev/null
say "$FAILED failed — report in $REPORT"
exit "$FAILED"
