# sill

**A shelf for AI coding agents on your own Linux machine.** Run Claude Code, Codex, Gemini or any
[ACP](https://agentclientprotocol.com) agent in a window that slides in on a hotkey, let it see your whole
machine, and keep every change it makes under your control: the folders you pin are live, everything else
the agent writes waits for your *Keep* or *Revert*. Sill remembers what each session did, so the next agent
in the same folder starts informed.

See [SPEC.md](SPEC.md) for the design and [docs/](docs/) for memory, security and design notes; this file
is the practical side. The [roadmap](ROADMAP.md) links ideas, proposed features, current work and history.

Status (2026-11-08): M0–M2 done and in daily use — sessions, pins, Changes, the file tree, a shell inside
the jail, managed processes, the ACP agent registry, standing instructions, tray icon, memory
(remember, recall, search), ghost sessions, permission modes (test desktop, private D-Bus, Host control),
and the foundations of remote use. The window is **sill-gui**
(Iced, [docs/sill-gui.md](docs/sill-gui.md)) — the only frontend since 2026-11-07, when the GTK shelf was
removed. Direction and open work: the [roadmap](ROADMAP.md).

## How it works

Three ideas carry the whole thing:

- **The jail.** Every session runs in its own bubblewrap sandbox. The agent sees the whole machine and
  can write anywhere, but unpinned writes land in an *overlay* on the state filesystem — the host is
  untouched until you say so. The network goes through sill's proxy with an allow-list in three layers:
  the policy's tooling hosts (npm, GitHub, crates.io, PyPI, Debian), the agent's own provider hosts from
  its definition (`api.anthropic.com` for Claude Code, `api.openai.com` for Codex; `domains` in the
  built-in definitions or the agent's TOML), and per-session grants — a refused host shows up as an
  *Allow* button in the conversation. Telemetry hosts are refused without asking. The agent's own login
  and config files (`~/.claude`, `~/.codex`, …) are sill-owned directories the agent's definition
  declares; your host's are never seen by an agent.
- **Pins.** A pinned folder is bound live into the jail: what the agent writes there is on the host at
  once, commits included, and you review with `git` as usual. Pins are the session's workspace and are
  named in every prompt as the place to work. Settings → *Remember pins* starts every new session with
  the latest session's pins, so the workspace carries over.
- **Changes.** Everything the agent wrote *outside* pins is listed, with diffs, per session and repo:
  Keep (copied to the host, the previous version saved so it can be undone), Revert, Keep all. Files you
  edited on the host meanwhile are marked as conflicts and left out of Keep all.

Plus **memory**: when a session ends, its agent writes a short summary as its last turn; sill stores it
with the facts (folders, kept files, repo heads, counts) and tells the next session in that folder what
came before. All of it is searchable from the host and inside the jail; sill-gui's Reopen list filters by
title and summary.

## Quick start

`sill` runs on any Linux box with bubblewrap, cgroup v2 and a kernel ≥ 6.2; systemd makes sessions
nicer (units, journal, limits) but is not required (SPEC §4.12).

```bash
# 1. Install — from the release tarball, the .deb, or source (see Build below). One root step does
#    everything an installer would, idempotently, and says what it did and skipped:
sudo sill init                 # sill, sill-shim, sill-gui to /usr/local · policy · icons · state image + units
sill init --user               # per account: sill-gui starts with your session; prints the hotkey hint

# 2. Open sill-gui
sill toggle                    # or the hotkey once you've set it (Settings → Window → Hotkey; Alt+Space is the usual choice)
```

Then, in sill-gui: `+` → pick an agent → it asks you to log in the first time (buttons in the
conversation; Claude Code's login runs in a terminal pane inside the jail). Pin the folder you want to
work in (the folder button opens the file tree; the pin button on a row, or the native folder picker,
pins it). Type. Enter sends, Shift+Enter is a newline.

When you're done, close the tab (Ctrl+W): the agent writes its summary, the jail stops, the
session stays in the Reopen list (its summary on hover).

Other `sill init` forms: `--dry-run` (the plan only), `--prefix /opt/sill`, `--state-dir /mnt/fast/sill`
(an own partition instead of the 32 GB image).

## Using sill-gui

**The herring** (top left; *sill* is Swedish for herring) flips between the sessions and the backend
pages — *Agents*, *Changes*, *Settings* (Settings → Default page picks which opens first).
Escape does the same flip; the fish flicks its tail on both.

**Sessions** are tabs. `+` picks the agent (Claude Code, Codex, Gemini, Antigravity, Kimi Code, anything
set up from the Agents page) — or its ghost — or reopens a stored one. Each session has its own jail, its
own conversation in `~/.local/state/sill/sill.db`, and resumes with history when reopened. Closing a
tab stops its jail; the session stays. The tab's glyph opens **Session details** (facts, summary,
notes); double-click the title to rename. The row under the prompt holds what the next turn uses:
`+` for attachments, the terminal, then the context ring and the agent's controls (mode, model,
reasoning), running processes, the **shield** (the session's Permissions panel: auto-allow — sill
answers "allow once" for you — jail access, approved hosts, Host control; right-click for the
shortcuts) and Send.

**Pins** show as chips above the prompt with their git +/−/untracked counts; a chip opens the file tree
scoped to that folder. Files can be pinned too.

**Changes** (herring → Changes) is every session's pending overlay writes, grouped by session and repo:
diff, Keep, Revert, Keep all, `!` for conflicts; plus the pinned repositories' own pending changes, and
each session's keeps with Undo. Keep checks that the file is still what you reviewed and that the host
copy hasn't changed since; Keep all takes plain adds and edits only — deletions, type changes and
symlinks take a click each. Nothing is kept automatically.

**The file tree** is a drawer beside the conversation: pinned folders, global pins, repositories with
changes, and home — git branch on a repo root, badges for what sessions changed, an absolute-path box.
A file opens in the viewer over the session: read (Markdown rendered, links followed), or *Edit* and
Ctrl+S. The drawer has a dragged width (Ctrl+Shift+D switches between the Shelf and Drawer layouts,
Ctrl+Shift+F into the Full layout and back).

**Shell**: the terminal icon (or Ctrl+Shift+T) swaps the prompt for a terminal running *inside the session's
jail* — same mounts, pins and proxy the agent has, so `git status`, builds and tests show the agent's
view. It starts in the first pinned folder, keeps running while hidden, dies with the session.
Right-click the icon (or Ctrl+Shift+H) for a shell on the host instead, marked `host · outside the jail`.

**Permissions and questions.** Permission requests are answered with buttons in the conversation. When
the agent asks you something with choices (Claude's "ask user question", an MCP elicitation) it appears
as a form: pick, or type your own under "Other", and Submit.

**Permission modes** ([docs/permission-modes.md](docs/permission-modes.md)). Every session starts
*Default*: the jail, no desktop, no bus. The shield's panel widens it per session, never saved as a
default: **Desktop development** (a Weston test desktop the agent can see and drive, shown live in
the window, plus a private D-Bus — *Apply development permissions* turns both on and restarts), and
**Host control**, an explicit, activation-scoped grant of real host operations (display, processes,
commands) with a red caption while it lasts; revoked by you, by the session ending or by the window
closing.

**Managed processes.** A command the agent needs to keep running after its turn — a dev server, a
watcher — is started inside the jail with `sill-proc start --name dev [--port 3000] -- npm run dev`
(`sill-proc list | tail dev | stop dev`). The shim keeps it alive across the agent's restarts; it dies
with the session. The controls row shows how many run, with state, last output line, log, Stop. A
process that declares a port is offered for exposing on this machine's loopback — asked once per
session, or silently for ports in the policy's `[processes] expose` list. Loopback only.

**Standing instructions** reach every agent, whatever its type: Settings → Instructions (Markdown,
`~/.config/sill/instructions.md`). Sill puts its own facts first — how the overlay and pins work, which
paths are private or read-only, the network allow-list, tool directories the jail's PATH lacks — then the
memory recall block, then your file. Sent once per agent start and again after an edit, not with every
prompt.

**Tray icon**: left click shows/hides the window, menu has Show/hide and *Quit the window* — the daemon
and its sessions go on; `sill toggle` or the hotkey brings the window back. While any agent works, the
icon moves (the sill ripples, the fish rides it); it is still when all are idle. Settings → Tray icon
turns it off.

**Images and files.** Paste an image into the prompt, or attach images and files from `+`; a pasted
image can be annotated (the pen) before sending. Images the agent produces show inline, with *Copy image*.

**Daemon status.** The activity indicator shows whether the daemon answers; Settings has diagnostics and
a daemon-only restart. `sill box` connects sill-gui to a daemon on another machine over SSH (foundations;
not yet exercised end to end).

## Memory

Sill remembers sessions on this machine — not *you*, and not the agent's own memory (Claude Code's
`~/.claude` memory files already persist through the shared agent home). `docs/memory.md` is the full
design; this is what you see.

- **Ending a session writes a summary.** Closing a tab with the agent up asks it one last prompt —
  one sentence on what the session was about and how it stands, then what is unfinished, what not
  to try again, what you decided, and a line on what was done — with the facts attached (pins, kept paths, repos and their heads). The tab stays while it answers
  (90 s cap; a second close ends at once with what it has). When the agent can't be asked, sill writes a
  mechanical summary from the facts. Either is stored on the session and marked which kind it is.
- **The Reopen list** (`+`) shows each ended session with its date or state and its name, the summary
  on hover, with a filter over titles and summaries. Clicking a row resumes the session with its
  history. *New here* (`+` on the row) starts a fresh session with the same agent, policy, scope and
  pins, told the old one's summary first; the bin deletes (on a second press).
- **Session details** (the tab's glyph) shows the facts (agent, when, how long, kept paths, folders),
  the summary — editable in place, saved as you type and marked *edited by the person* — and your
  **notes**: one line each, Enter adds, × removes; they are searched and recalled with the summary.
  *Summarize now* on an open session asks the agent for a checkpoint summary that replaces the text;
  the one written when the session ends replaces that (close the tab mid-checkpoint and the
  checkpoint becomes the final summary).
- **Recall.** The next session in a folder gets a `[Memory — earlier sessions in these folders …]`
  block in its standing instructions: the last three ended sessions, each with date, duration, agent,
  folders, kept files, commits and the summary. Settings → Memory → **Recall** picks the scope:
  *this folder* (the same folder, its parents and children, nearest first — the default with pins),
  *all folders* (the last three anywhere — the default without pins, so a no-pin session is the one
  that sees all of memory), or *none*. Scope decides what is pushed, never what can be searched —
  the block tells the agent that search covers all sessions.
  `memory_recall` in `sill.toml` changes the count.
- **Search from anywhere.** On the host, `sill memory list | show <id|title> | search <words> |
  transcript <id> [--grep WORDS] | recall <id> | refresh`. Inside the jail the agent has
  `sill-memory search | show | transcript` over a read-only snapshot (`/.sill/memory/memory.db`,
  refreshed when a session ends or is deleted, and by `sill memory refresh`) — it can read memory, never write it. Full-text
  search (SQLite FTS5, no embeddings) over titles, summaries, notes and prompts; agent text through
  `transcript --grep`.
- **Off switch**: Settings → Memory. With it off nothing is summarised or recalled.

**Ghost sessions** (the ghost button on an agent's row in `+`, or `sill session new --ghost`) leave
nothing behind: writes revert after an hour, no pins, an isolated agent home with only the declared
credentials, transcript and overlay deleted at close, nothing written to memory. Settings → Memory →
*Ghosts see memory* decides whether they read it.

## Agents

**Agents page** (herring → Agents): the ACP registry (`cdn.agentclientprotocol.com`, cached, refreshed
when older than six hours) with search, filters (All / Installed / Not installed, *Untested*) and one
action per entry. *Install* puts the package under `~/.local/state/sill/tools/<id>/` (npm with
`--ignore-scripts`, uv with its own tool dir, or the Linux binary with its checksum verified), probes it
once over ACP in a throwaway jail, and writes `~/.config/sill/agents/<id>.toml`, so it appears in the
`+` menu with its icon; *Latest* / *Supported* picks the version. The settings card of an installed
agent edits its command, environment (API keys), proxy domains and **Directories** — the paths under
`~` that are its own. Third-party agents start with an empty allow-list: when one tries a host the
proxy refuses, the conversation asks whether to allow that host (or its domain) for the agent, and the
running session is granted it live.

**Agents' own files** ([docs/agent-directories.md](docs/agent-directories.md)). Each agent runs
completely inside sill: the paths its definition declares (`~/.claude` and `~/.claude.json`, `~/.codex`,
`~/.gemini`, …) are replaced in every jail by sill-owned directories under
`~/.local/state/sill/agents/<id>/`; nothing is copied from the host and other agents' homes are masked.
Claude Code, Codex, Gemini, Antigravity and Kimi Code have built-in definitions; a registry agent
without one is set up first on its settings card — *Find directories* (a probe), a login that records
where it wrote, or the Directories editor. Caches (`~/.cache`, `~/.npm`) are per-session directories on
the state filesystem, never on the host. Delete `~/.local/state/sill/agents/<id>` to make the agent
log in again.

**Logging in.** When an agent refuses to open a session without a login, the conversation shows one
button per method. Claude Code logs in in sill-gui's terminal pane inside the jail — its own login,
separate from the host's Claude Code or Zed. Codex with an API key: put `OPENAI_API_KEY` in its
environment and pick *API Key*; with ChatGPT pick *ChatGPT* (or `sill login codex`) — that login runs
on this machine because its browser callback can't reach the jail, but with `CODEX_HOME` pointing at
the jail's copy, so nothing lands on the host. Registry agents log in from their settings card, in a
terminal pane where the agent needs one (Kimi Code). Gemini: see Known limits.

**Policy changes.** A jail gets its mounts and permission state when it is launched. After editing the
policy (`/etc/sill/policies/default.toml`, then `~/.config/sill/policies/`) the running session's shield
says *changes apply after a restart* and offers *Restart now*; Settings → Policies inspects and
refreshes the installed policies. Restarting sill-gui alone does not touch running sessions.

## The command line

```bash
sill gui [--hidden]                        # start sill-gui (a second start shows the running window)
sill toggle                                # show / hide sill-gui (bind it to a key)
sill restart [--dev] [--hidden] [--jails]  # after an update: the daemon and sill-gui (--dev: the checkout's target/debug build; --jails: the sessions too)
sill daemon status | restart | stop | schema
sill session new [--agent A] [--pin P] [--ghost] | list | end <id>
sill changes list | diff | keep | revert | history | undo
sill pin list | add <path> | remove <path>  # global pins
sill memory …                              # see Memory above
sill policy show | preview | snapshot | check | import
sill permissions show | set | development | restore | save-host
sill host-control activate | revoke | keep | state
sill registry … · sill login <agent> · sill box add | list | remove | connect · sill attach
journalctl --user -u sill-gui -f           # sill-gui's log
```

`sill <cmd> --help` has the arguments.

The jail without sill-gui — handy for experiments and scripting. Until the state filesystem is
mounted, point `--state-dir` at a directory that is not inside an overlaid tree and pass
`--unsafe-state` (`/tmp` works; it's tmpfs, so copy-ups cost RAM).

```bash
S=target/debug/sill
$S jail plan --unsafe-state --state-dir /tmp/s1 -- true      # the bwrap command line
$S jail run  --unsafe-state --state-dir /tmp/s1              # a shell in a jail: whole machine visible,
                                                             # every write recorded, no network

# a development session: jail + shim + proxy, detached
$S dev start --unsafe-state --state-dir /tmp/s1 --cwd ~/some/repo
$S dev status
$S dev prompt "Create hello.txt with one line: hi"           # Claude Code runs inside the jail
$S jail changes --state-dir /tmp/s1                          # A/M/D list against the host
$S jail changes --state-dir /tmp/s1 --diff ~/some/repo/hello.txt
$S jail keep    --state-dir /tmp/s1 ~/some/repo/hello.txt    # copy to the host (its previous version is saved)
$S jail kept    --state-dir /tmp/s1                          # what was kept and can still be undone
$S jail unkeep  --state-dir /tmp/s1 ~/some/repo/hello.txt    # put the host's previous version back
$S dev revert ~/some/repo/other.txt                          # restore from the host, inside the jail
$S dev stop
```

The dev session starts the way the daemon starts sill-gui's sessions (docs/daemon.md §3.1): in a
transient scope `sill-dev.scope` under `sill.slice` with the policy's limits where the systemd user
instance answers, its log in the journal (`journalctl --user -t sill-dev`, the proxy's allow/deny
lines included); otherwise — or with `--direct`, or `SILL_LAUNCHER=direct` — in a cgroup made by hand
under the `sill init` subtree, logging to `$XDG_RUNTIME_DIR/sill/dev/jail.log`. Without either there
are no resource limits: the default policy refuses to start then (`[limits] required = true`); set
it to `false` to run anyway with a warning. `--net` shares the host network (testing only).

## Build

```bash
# Debian 13: rustup stable (Debian's 1.85 is too old), bubblewrap, and what iced needs to build; no GTK
curl -sSf https://sh.rustup.rs | sh
sudo apt install bubblewrap libwayland-dev libxkbcommon-dev
cargo build --release
cargo test --workspace
packaging/deploy.sh --fast     # quicker local builds; incremental cache in target/local
packaging/deploy.sh            # from source, in a host terminal (Ctrl+Shift+H): build, sudo sill init, restart, verify
packaging/check.sh             # deploy.sh runs it last: daemon, units, a jail's view, keep/undo, a ghost session → target/host-check/report.txt
```

For frequent local changes, `--fast` uses modest optimization, 16 code-generation units,
no LTO, and incremental compilation. The first build fills a separate cache; later builds
reuse it. Use `cargo build --profile local` to build without installing. Ordinary release
builds retain their full optimization settings.

Release channels: the tarball (`sill-<ver>-<arch>.tar.gz` + `SHA256SUMS`, `packaging/release.sh`)
with `sudo ./sill init` inside; the `.deb` (`packaging/deb.sh`, `postinst` runs `sill init`); from
source, `cargo install sill sill-shim sill-gui` then `sudo sill init` (binaries are taken from next to the
running `sill`). Signed sums, `sill.io` and the apt repository come later.

| Path | What |
|---|---|
| `crates/sill-core` | library: policies, mount table, jail builder (bwrap argv), ledger, keep, memory, shim client, ACP client, proxy |
| `crates/sill-shim` | the supervisor inside the jail: agent stdio relay, control channel, revert, proxy forwarder, `sill-proc`, `sill-memory` |
| `crates/sill-memory` | the FTS5 index, the read-only snapshot and the search queries |
| `crates/sill` | the `sill` command and the daemon |
| `crates/sill-gui` | the desktop window (Iced); the fonts under `crates/sill-gui/fonts/` |
| `crates/sill-client` | the toolkit-free frontend core sill-gui is built on |
| `crates/sill-proto` | the daemon protocol (SFP): wire types, schema, the client |
| `policies/default.toml` | the built-in policy, also installed to `/etc/sill/policies/` |
| `packaging/systemd/` | state-filesystem units (`/var/lib/sill.img` → `/var/lib/sill`), sill-gui's user service |
| `packaging/release.sh`, `packaging/deb.sh` | the release tarball + `SHA256SUMS`, the `.deb`; `packaging/sill.desktop`, `packaging/icons/` |
| `docs/` | the window (`sill-gui.md`), the daemon and protocol (`daemon.md`, `protocol.md`, `sfp-operations.md`), `permission-modes.md`, `agent-directories.md`, `policy-inspection.md`, `pins.md`, `login.md`, `memory.md`, `security.md`, `design.md`; proposals and history (`proposed-features.md`, `tasks.md`, `crosscheck.md`, `session-overview.md`, `parallel-coding.md`, `work-items.md`, `development-history.md`) |

## State filesystem

Session upper trees must not live inside an overlaid tree (SPEC §4.9). `sill init` creates a sparse
32 GB ext4 image `/var/lib/sill.img` and mounts it at `/var/lib/sill` (sticky, like `/tmp`; each user
gets a 0700 dir) — through the units in `packaging/systemd/` where systemd exists, through an
`/etc/fstab` line elsewhere; `--state-dir` bind-mounts an own partition there instead. With that
mounted, `sill` uses `/var/lib/sill/<uid>/sessions/<id>` by default. Grow the image: stop the mount,
`truncate -s 64G`, `e2fsck -f`, `resize2fs`, start it again.

## Known limits

- Files not owned by your uid:gid can't be modified in the jail (overlayfs copy-up can't chown to an
  unmapped id; SPEC §4.10 #16). Group-shared trees: `direct` or `ro` in the policy for now, or pin them.
- A parked agent process costs ~1.6 % of a core; the policy's `agent_idle_minutes` (default 10) stops
  it and the session resumes it on the next prompt.
- HTTP CONNECT proxy only (no SOCKS5); programs that ignore `HTTPS_PROXY` have no network, by design.
- Gemini CLI's "Log in with Google" runs host-side like Codex's ChatGPT login, but Google stopped serving
  personal accounts through Gemini CLI on 2026-06-18. Use a Gemini API key (`GEMINI_API_KEY` in its
  environment, then *Gemini API key*), or Antigravity (built in; install it from the Agents page).
- Sessions end — and are summarised — on a tab close and the idle timeout; `sill restart` and the
  tray's Quit end none. A tab closed again while its agent writes the summary disappears at once and
  the summary finishes in the background (still capped at 90 s).

## Look

Sill's look is its own and the same on every machine: its icons are drawn glyphs (the mark too — the
herring in a jar, `packaging/icons/sill.svg`; the tray's is the herring on its sill), every theme is sill's own, a
complete palette (nothing follows the desktop's theme, icon theme or dark/light setting; no GTK, no
libadwaita), and the text is set in two faces embedded in the binary — **Source Sans 3** for everything
proportional, **Source Code Pro** for terminals, code and the editor (both by Adobe, SIL Open Font
License 1.1; the licence texts ship under `crates/sill-gui/fonts/`). Text scales with the interface
scale only (Settings, or Ctrl+Plus / Ctrl+- / Ctrl+0). `docs/design.md` says why.

## Keys

| Key | What |
|---|---|
| the hotkey (Settings → Window → Hotkey; Alt+Space is the usual choice — or bind `sill toggle` in the desktop's keyboard settings) | show / hide the window |
| Ctrl+T / Ctrl+W | a new session / close the session (writes the summary) |
| Ctrl+, / Ctrl+. | previous / next tab |
| Ctrl+1 … Ctrl+9 | tab N |
| Ctrl+Shift+T | the terminal inside the session's jail |
| Ctrl+Shift+H | a shell on the host, marked `host · outside the jail` |
| Ctrl+Shift+D | Shelf ⇄ Drawer layout |
| Ctrl+Shift+F | Full layout |
| Ctrl+Plus / Ctrl+- / Ctrl+0 | interface scale |
| Escape | the sessions ⇄ the backend pages |
| Enter / Shift+Enter | send / newline |
| Ctrl+Q | hides the window |

The session and layout shortcuts are editable in Settings → Shortcuts; the hotkey has its own field;
Escape, Enter and Ctrl+Q are fixed.
