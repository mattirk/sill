#!/bin/sh
# List only this account's local frontend processes. Remote windows have their own lifetime.
# Optional proc root is useful for checking this parser against fixtures.
set -eu
sill_proc_root=${1:-/proc}
sill_uid=$(id -u)
for sill_proc in "$sill_proc_root"/[0-9]*; do
    [ -d "$sill_proc" ] || continue
    [ "$(stat -c %u "$sill_proc" 2>/dev/null)" = "$sill_uid" ] || continue
    [ -r "$sill_proc/cmdline" ] && [ -r "$sill_proc/environ" ] || continue
    # Read environment values privately: only SILL_BOX affects selection, and only argv is printed.
    awk -v pid="${sill_proc##*/}" '
        BEGIN { RS="\0"; box="local" }
        FILENAME == ARGV[1] { if (index($0,"SILL_BOX=")==1) box=substr($0,10); next }
        { args[++n]=$0 }
        END {
            executable=args[1]; sub(/^.*\//,"",executable)
            if (executable!="sill") exit
            for (i=2;i<=n;i++) {
                if (args[i]=="--box") { i++; if (i>n) exit; box=args[i] }
                else if (index(args[i],"--box=")==1) box=substr(args[i],7)
                else if (args[i]!="") positional[++m]=args[i]
            }
            if (positional[1]=="box" && positional[2]=="connect" && positional[3]!="") box=positional[3]
            else if (positional[1]!="shelf") exit
            if (box!="" && box!="local") exit
            line=pid
            for (i=1;i<=n;i++) if (args[i]!="") line=line " " args[i]
            print line
        }
    ' "$sill_proc/environ" "$sill_proc/cmdline" 2>/dev/null || true
done
