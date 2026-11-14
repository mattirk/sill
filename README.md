# sill

A shelf for AI agents on your own Linux machine: an ACP client, a jail the host controls, and a ledger
of every change the agent makes. See [SPEC.md](SPEC.md) for the design; this file is the practical side.

Status: **M2** (2026-10-28). Sessions with tabs, persistence and resume; folder pins per session (live,
host = jail); the `sill ▾` dropdown with the global Changes page (keep / revert / conflicts), the Files page,
Settings. Next: M3 — a terminal inside the session's jail, policies UI, more agents.

## Layout

| Path | What |
|---|---|
| `crates/sill-core` | library: policies, mount table, jail builder (bwrap argv), ledger, keep, shim client, ACP client, proxy |
| `crates/sill-shim` | the supervisor that runs inside the jail: agent stdio relay, control channel, revert, proxy forwarder |
| `crates/sill` | the `sill` command and the GTK shelf (`crates/sill/src/shelf/`) |
| `policies/default.toml` | the built-in policy, also installed to `/etc/sill/policies/` |
| `packaging/systemd/` | state-filesystem units (`/var/lib/sill.img` → `/var/lib/sill`), the shelf's user service; `sill init` embeds and installs them |
| `packaging/release.sh`, `packaging/deb.sh` | the release tarball + `SHA256SUMS`, the `.deb`; `packaging/sill.desktop`, `packaging/icons/` |

## Build

```bash
# Debian 13: rustup stable (Debian's 1.85 is too old), bubblewrap
curl -sSf https://sh.rustup.rs | sh
sudo apt install bubblewrap
cargo build
cargo test
```

The shelf needs `libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libgtksourceview-5-dev`.

## Install

`sill` runs on any Linux box with bubblewrap, cgroup v2 and a kernel ≥ 6.2; systemd makes sessions
nicer (units, journal, limits) but is not required (SPEC §4.12). One root step does everything an
installer or a package would, idempotently, and says what it did and what it skipped:

```bash
sudo sill init                 # checks · binaries to /usr/local · policy · icons · state image + units/fstab
sudo sill init --dry-run       # the plan only
sudo sill init --prefix /opt/sill
sudo sill init --state-dir /mnt/fast/sill    # an own partition instead of the 32 GB image
sill init --user               # per account: the shelf starts with your session; prints the hotkey hint
```

Channels: the release tarball (`sill-<ver>-<arch>.tar.gz` + `SHA256SUMS`, `packaging/release.sh`) with
`sudo ./sill init` inside; the `.deb` (`packaging/deb.sh`, `postinst` runs `sill init`); from source,
`cargo install sill-shelf sill-shim` then `sudo sill init` (binaries are taken from next to the running
`sill`). On studio-debian `studio-debian-system/sound/config/install.sh` runs `sill init` from the
repo's release build. Signed sums, `sill.io` and the apt repository come later.

## The jail from the command line

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

The dev session starts the way the shelf's sessions do (SPEC §4.7): as the transient user unit
`sill-dev.service` where the systemd user instance is reachable (logs: `journalctl --user -u sill-dev`),
otherwise — or with `--direct`, or `SILL_LAUNCHER=direct` — as a detached process with pids and logs
in `$XDG_RUNTIME_DIR/sill/dev/` (`{launch,bwrap}.pid`, `{jail,proxy}.log`; the proxy log lists every
allow/deny). Direct mode without a delegated cgroup (`sudo sill init` makes one on hosts without
systemd) has no resource limits: the default policy refuses to start then (`[limits] required = true`);
set it to `false` to run anyway with a warning. `--net` shares the host network instead of using the
proxy (testing only).

## The shelf

```bash
sill init --user                           # once: the shelf starts with the session (user service, or
                                           # ~/.config/autostart/sill.desktop without systemd)
systemctl --user restart sill-shelf        # after an update (same as `sill restart`)
journalctl --user -u sill-shelf -f         # its log
sill toggle                                # show/hide — bind to Alt+Space (Cinnamon custom shortcut)
sill restart --dev                         # run the build in target/ instead of the installed one
```

- **`sill`** (top left) flips between the sessions and the backend: the session tabs give way to page tabs
  (Changes · Files · Agents · Settings), opening on the default page (Changes; Settings → Default page).
  `sill` again returns to the session; Escape does the same flip either way. The folder button opens Files
  and, pressed again, returns to the session.
- **Sessions** are tabs: `+` picks the agent (Claude Code, Codex, Gemini; `~/.config/sill/agents/*.toml`
  adds more, with an `icon` name; sill ships provider glyphs `sill-claude-symbolic`, `sill-openai-symbolic`,
  `sill-gemini-symbolic` and a neutral placeholder for anything else) or reopens a stored one. Each session has its own jail (`sill-<id>.service`, upper trees under
  `/var/lib/sill/<uid>/sessions/<id>`), its own conversation stored in `~/.local/state/sill/sill.db`, and
  resumes with history (`session/load`) when reopened. Closing a tab stops its jail; the session stays.
  The tab's glyph (or a right-click on the tab) opens the **session menu**: name, policy, desktop access
  (X11 / D-Bus — the glyph turns amber while granted) and where the jail runs; a policy or desktop change
  rebuilds the jail. The row under the prompt holds only what the next turn uses.
- **Pins** are the session's workspace. A pinned folder is bound live into the jail: what the agent
  writes there is on the host at once, commits included, and review is `git` as usual. The chips above the
  prompt show each pin with its +/−/untracked counts; the pins are also named in every prompt as the place
  to work. Everything *unpinned* is overlaid: the agent sees the whole machine and can write anywhere, but
  the writes land in the session's upper tree and wait in Changes. Nothing is kept automatically.
- The **`sill ▾`** button switches pages: *Sessions*, *Changes* (every session's pending overlay changes,
  grouped by session and repo: diff, Keep, Revert, Keep all; conflicts — edited on the host meanwhile —
  marked `!` and left out of Keep all), *Files* (tree of touched repos with badges), *Settings*. Escape
  goes back, then hides.
- Enter sends, Shift+Enter newline. Permissions are answered with the buttons in the conversation; the
  lock icon in the controls row is auto-allow (open = sill answers "allow once" for you).
- **Shell**: the terminal icon (or Ctrl+`) swaps the prompt for a terminal running *inside the session's
  jail* — same mounts, pins and proxy the agent has, so `git status`, builds and tests show the agent's
  view. It starts in the first pinned folder, keeps running while hidden, and dies with the session.

Per-user setup (done): `sill-shelf.service` enabled, Cinnamon shortcut `custom0` =
`/usr/local/bin/sill toggle`. Binaries are installed system-wide by `sudo sill init` (the studio
install script runs it from `target/release/`; build with `cargo build --release` first). Settings
shows which launcher mode the shelf found (unit / direct) and where a session's logs are.

## State filesystem

Session upper trees must not live inside an overlaid tree (SPEC §4.9). `sill init` creates a sparse
32 GB ext4 image `/var/lib/sill.img` and mounts it at `/var/lib/sill` (sticky, like `/tmp`; each user
gets a 0700 dir) — through the units in `packaging/systemd/` where systemd exists, through an
`/etc/fstab` line elsewhere; `--state-dir` bind-mounts an own partition there instead. With that
mounted, `sill` uses `/var/lib/sill/<uid>/sessions/<id>` by default and `--unsafe-state` is no longer
needed. Grow the image: stop the mount, `truncate -s 64G`, `e2fsck -f`, `resize2fs`, start it again.

## Agent home and private scratch

Each agent runs completely inside sill: its own paths (`~/.claude` and `~/.claude.json` for Claude Code,
`~/.codex`, `~/.gemini`) are replaced in every jail by a sill-owned copy under
`~/.local/state/sill/agents/<id>/`, seeded once with the login files from the host. The host's own
`~/.claude` is never seen or written by an agent. Caches (`~/.cache`, `~/.npm`: policy `private`) are
per-session directories on the state filesystem, never on the host. Delete
`~/.local/state/sill/agents/<id>` to make the agent log in again.

**Agents page** (sill ▾ → Agents): the ACP registry (`cdn.agentclientprotocol.com`, cached in
`~/.cache/sill`, refreshed when older than six hours) with search, an All / Installed / Not installed filter and
one action per entry. *Install* puts the package under `~/.local/state/sill/tools/<id>/` (npm with
`--ignore-scripts`, uv with its own tool dir, or the Linux binary with its checksum verified), probes it once
over ACP for session resume, and writes `~/.config/sill/agents/<id>.toml`, so it appears in the `+` menu with
its registry icon. Entries that are the built-ins' own packages (Claude Code, Codex, Gemini) keep the
built-in's jail settings and only take the new command. The gear on an installed row edits its command,
environment (API keys) and proxy domains. Third-party agents start with an empty allow-list: when one tries a
host the proxy refuses, the conversation asks whether to allow that host (or its whole domain) for the agent,
and the session restarts with it allowed. The trash icon removes an install again.

**Agents' own files.** Each agent's state lives in `~/.local/state/sill/agents/<id>/`, never on the host and
never in the change ledger. The built-ins declare their paths; anything else is learned: a new dot-path an agent
creates under `~` (`~/.foo`, `~/.config/foo`) is adopted into its sill home after the turn and bound there in
later sessions, and host-side logins of such agents write straight into that home.

**Logging in.** When an agent refuses to open a session without a login, the conversation shows one button
per method the agent offers. Claude Code's logins run in the shelf's terminal pane inside the jail. Codex
with an API key: put `OPENAI_API_KEY` in its environment (the gear on the Agents page) and pick *API Key*. Codex with
ChatGPT: pick *ChatGPT* (or run `sill login codex`); that login runs on this machine because its browser
callback can't reach into the jail, but with `CODEX_HOME` pointing at the jail's own `~/.codex` copy, so
nothing lands on the host; the session then restarts. Claude Code logs in inside sill too (its terminal login in the
shell pane), as a login of its own, separate from the host's Claude Code or Zed.

**Policy changes.** A jail gets its mounts and desktop access (`x11`, `dbus`) when it is launched. After
editing the policy, reopen a session's tab (or close and reopen it): a jail older than the policy file is
rebuilt automatically. Restarting the shelf alone does not touch running sessions.

## Known limits

- Files not owned by your uid:gid can't be modified in the jail (overlayfs copy-up can't chown to an
  unmapped id; SPEC §4.10 #16). Group-shared trees: `direct` or `ro` in the policy for now, or pin them.
- The parked agent process costs ~1.6 % of a core; close the tab when idle until idle-stop exists.
- HTTP CONNECT proxy only (no SOCKS5); programs that ignore `HTTPS_PROXY` have no network, by design.
- Gemini CLI's "Log in with Google" runs host-side like Codex's ChatGPT login, but Google stopped serving
  personal accounts (free, AI Pro, Ultra) through Gemini CLI on 2026-06-18 and answers with "This client is no
  longer supported for Gemini Code Assist for individuals". Use a Gemini API key (`GEMINI_API_KEY` in its
  environment, then *Gemini API key*), or install Antigravity from the Agents page.
- No terminal in the jail, no policies UI, no focus keeper yet (M3).
