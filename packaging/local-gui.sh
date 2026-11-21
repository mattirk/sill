#!/bin/sh
# List only this account's local silli-gui processes: pid and argv, one per line. A window opened on a
# saved box (SILLI_BOX in its environment) has its own lifetime and is left out.
# Optional proc root is useful for checking this parser against fixtures.
set -eu
silli_proc_root=${1:-/proc}
silli_uid=$(id -u)
for silli_proc in "$silli_proc_root"/[0-9]*; do
    [ -d "$silli_proc" ] || continue
    [ "$(stat -c %u "$silli_proc" 2>/dev/null)" = "$silli_uid" ] || continue
    [ -r "$silli_proc/cmdline" ] && [ -r "$silli_proc/environ" ] || continue
    # Read environment values privately: only SILLI_BOX affects selection, and only argv is printed.
    awk -v pid="${silli_proc##*/}" '
        BEGIN { RS="\0"; box="local" }
        FILENAME == ARGV[1] { if (index($0,"SILLI_BOX=")==1) box=substr($0,10); next }
        { args[++n]=$0 }
        END {
            executable=args[1]; sub(/^.*\//,"",executable)
            if (executable!="silli-gui") exit
            if (box!="" && box!="local") exit
            line=pid
            for (i=1;i<=n;i++) if (args[i]!="") line=line " " args[i]
            print line
        }
    ' "$silli_proc/environ" "$silli_proc/cmdline" 2>/dev/null || true
done
