# sill

A shelf for AI agents on your own Linux machine: an ACP client, a jail the host controls, and a ledger
of every change the agent makes. See [SPEC.md](SPEC.md) for the design; this file is the practical side.

Status: **M0** (2026-10-28). The risky parts work end to end from the command line; no GTK shelf yet.

## Layout

| Path | What |
|---|---|
| `crates/sill-core` | library: policies, mount table, jail builder (bwrap argv), ledger, keep, shim client, ACP client, proxy |
| `crates/sill-shim` | the supervisor that runs inside the jail: agent stdio relay, control channel, revert, proxy forwarder |
| `crates/sill` | the `sill` command (the GTK shelf will live here too) |
| `policies/default.toml` | the built-in policy, also installed to `/etc/sill/policies/` |
| `packaging/systemd/` | state-filesystem units (`/var/lib/sill.img` → `/var/lib/sill`) |

## Build

```bash
# Debian 13: rustup stable (Debian's 1.85 is too old), bubblewrap
curl -sSf https://sh.rustup.rs | sh
sudo apt install bubblewrap
cargo build
cargo test
```

GTK is not needed until M1 (`libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libgtksourceview-5-dev`).

## Try it (M0)

Until the state filesystem is mounted, point `--state-dir` at a directory that is not inside an overlaid
tree and pass `--unsafe-state` (`/tmp` works for experiments; it's tmpfs, so copy-ups cost RAM).

```bash
S=target/debug/sill
$S policy                                  # the policy as sill reads it
$S jail plan --unsafe-state --state-dir /tmp/s1 -- true      # the bwrap command line
$S jail run  --unsafe-state --state-dir /tmp/s1              # a shell in a jail: whole machine visible,
                                                             # every write recorded, no network

# a development session: jail + shim + proxy, detached
$S dev start --unsafe-state --state-dir /tmp/s1 --cwd ~/some/repo
$S dev status
$S dev prompt "Create hello.txt with one line: hi"           # Claude Code runs inside the jail
$S jail changes --state-dir /tmp/s1                          # A/M/D list against the host
$S jail changes --state-dir /tmp/s1 --diff ~/some/repo/hello.txt
$S jail keep    --state-dir /tmp/s1 ~/some/repo/hello.txt    # copy to the host
$S dev revert ~/some/repo/other.txt                          # restore from the host, inside the jail
$S dev stop
```

Logs: `$XDG_RUNTIME_DIR/sill/dev/{jail,proxy}.log` (the proxy log lists every allow/deny).
`--net` shares the host network instead of using the proxy (testing only).

## State filesystem

Session upper trees must not live inside an overlaid tree (SPEC §4.9). The units in
`packaging/systemd/` create a sparse 32 GB ext4 image and mount it at `/var/lib/sill` (sticky, like
`/tmp`; each user gets a 0700 dir). On studio-debian `studio-debian-system/sound/config/install.sh`
installs and enables them; elsewhere:

```bash
sudo install -m 644 packaging/systemd/* /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now var-lib-sill.mount sill-state-perms.service
```

With that mounted, `sill` uses `/var/lib/sill/<uid>/sessions/<id>` by default and `--unsafe-state` is
no longer needed.

## Known limits (M0)

- Files not owned by your uid:gid can't be modified in the jail (overlayfs copy-up can't chown to an
  unmapped id; SPEC §4.10 #16). Group-shared trees: `direct` or `ro` in the policy for now.
- The parked agent process costs ~1.6 % of a core; stop it when idle (`dev stop`) until idle-stop exists.
- HTTP CONNECT proxy only (no SOCKS5); programs that ignore `HTTPS_PROXY` have no network, by design.
- The jail is detached with `setsid`, not yet a systemd user unit.
