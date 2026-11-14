# sill

**A shelf for AI coding agents.** Sill runs Claude Code, Codex, Kimi Code or any
[ACP](https://agentclientprotocol.com) agent in a jail on your own machine or remote box. The agent sees
the whole machine, but what it may change, reach and keep is up to you.

- **Security first.** Every session runs in its own sandbox (bubblewrap, no capabilities, its own
  network namespace). Secrets such as `~/.ssh` are hidden, and the network is an allow-list behind
  sill's proxy.
- **Pins and Changes.** Folders you pin are live: the agent works in them directly. Everything it writes
  elsewhere waits in an overlay for your *Keep* or *Revert*, with diffs.
- **Permissions on request.** A test desktop, a private D-Bus or real host operations are granted per
  session and revoked when it ends.
- **Memory.** Each session leaves a summary, so the next agent in the same folder starts informed.
- **VM and remote.** The daemon and its jails can run on your Linux desktop, in a local Linux VM
  (Sill.app on Apple Silicon Macs) or on another Linux box over SSH.

Sill reduces what an agent can do to your machine; it does not guarantee security. The jail shares the
host's kernel unless you use the VM, pinned folders are written directly, and the code has not had an
independent audit.

Sill is developed with AI coding agents, run inside sill itself. The design, the direction and the
review of what is merged are the author's. Review the code yourself before relying on it.

## Run

**Linux desktop** — bubblewrap, cgroup v2, kernel ≥ 6.2, Rust ≥ 1.90:

```bash
sudo apt install bubblewrap libwayland-dev libxkbcommon-dev
cargo build --release
sudo target/release/sill init     # binaries, policy, state filesystem
sill init --user                  # start the window with your session
sill toggle                       # show it (or set a hotkey in Settings)
```

**Mac** (Apple Silicon) — builds a Debian VM image and the app around it:

```bash
packaging/mac-image.sh
packaging/mac/app.sh              # dist/Sill.app, dist/Sill.dmg
```

**Remote box** — on a Debian server (same requirements, plus systemd and an SSH server), from a clone,
as the account that will own the sessions and may use sudo:

```bash
packaging/deploy-server.sh
```

Then add it in Settings → Remote boxes on your desktop. The connection is SSH; no port is opened.

## Architecture

A daemon owns the sessions; each session is a jail with a supervisor inside it; the window is a client
of the daemon's protocol, locally or over SSH.

| Crate | What |
|---|---|
| `sill` | the `sill` command and the daemon |
| `sill-core` | policies, the jail builder, the overlay ledger, Keep, the network proxy, the ACP client |
| `sill-shim` | the supervisor inside the jail: agent relay, managed processes, in-jail tools |
| `sill-proto` | the daemon protocol (SFP): wire types, schema, a blocking client |
| `sill-client` | toolkit-free frontend core: session views, reconnection |
| `sill-gui` | the desktop window (Iced) |
| `sill-memory` | session memory: index and search, on the host and in the jail |
| `sill-vm` | the local Linux VM on macOS (Apple's Virtualization framework) |

Also: `policies/` (the default policy) and `packaging/` (install, release and Mac scripts).

## License

Apache-2.0; see [LICENSE](LICENSE). The patched copy of `iced_widget` under `vendor/` is MIT, and
the fonts in `crates/sill-gui/fonts/` are under the SIL Open Font License; their notices are beside them.
