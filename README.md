# sill

**A shelf for AI coding agents on your own Linux machine.** Run Claude Code, Codex, Gemini or any
[ACP](https://agentclientprotocol.com) agent in a window that slides in on a hotkey, let it see your whole
machine, and keep every change it makes under your control: the folders you pin are live, everything else
the agent writes waits for your *Keep* or *Revert*. Sill remembers what each session did, so the next agent
in the same folder starts informed.

See [SPEC.md](SPEC.md) for the design and [docs/](docs/) for memory, security and design notes; this file
is the practical side.

Status (2026-11-01): M0–M2 done and in daily use — sessions, pins, Changes, the file tree, a shell inside
the jail, managed processes, the ACP agent registry, standing instructions, tray icon, and memory
(remember, recall, search). Next: memory compaction and retention, ghost sessions, policies UI.

## How it works

Three ideas carry the whole thing:

- **The jail.** Every session runs in its own bubblewrap sandbox. The agent sees the whole machine and
  can write anywhere, but unpinned writes land in an *overlay* on the state filesystem — the host is
  untouched until you say so. The network goes through sill's proxy with an allow-list; a refused host
  shows up as an *Allow* button in the conversation. The agent's own login and config files
  (`~/.claude`, `~/.codex`, …) are sill-owned copies; your host's are never seen by an agent.
- **Pins.** A pinned folder is bound live into the jail: what the agent writes there is on the host at
  once, commits included, and you review with `git` as usual. Pins are the session's workspace and are
  named in every prompt as the place to work.
- **Changes.** Everything the agent wrote *outside* pins is listed, with diffs, per session and repo:
  Keep (copied to the host, the previous version saved so it can be undone), Revert, Keep all. Files you
  edited on the host meanwhile are marked as conflicts and left out of Keep all.

Plus **memory**: when a session ends, its agent writes a short summary as its last turn; sill stores it
with the facts (folders, kept files, repo heads, counts) and tells the next session in that folder what
came before. All of it is searchable, from the shelf, the host and inside the jail.

## Quick start

`sill` runs on any Linux box with bubblewrap, cgroup v2 and a kernel ≥ 6.2; systemd makes sessions
nicer (units, journal, limits) but is not required (SPEC §4.12).

```bash
# 1. Install — from the release tarball, the .deb, or source (see Build below). One root step does
#    everything an installer would, idempotently, and says what it did and skipped:
sudo sill init                 # binaries to /usr/local · policy · icons · state image + units
sill init --user               # per account: the shelf starts with your session; prints the hotkey hint

# 2. Open the shelf
sill toggle                    # or Alt+Space once you've bound it (Settings → Hotkey)
```

Then, in the shelf: `+` → pick an agent → it asks you to log in the first time (buttons in the
conversation; Claude Code's login runs in a terminal pane inside the jail). Pin the folder you want to
work in (the folder button opens the file tree; `+` on a row pins it). Type. Enter sends,
Shift+Enter is a newline.

When you're done, close the tab (Ctrl+Shift+W): the agent writes its summary, the jail stops, the
session stays in the Reopen list with its summary under its name.

Other `sill init` forms: `--dry-run` (the plan only), `--prefix /opt/sill`, `--state-dir /mnt/fast/sill`
(an own partition instead of the 32 GB image).

## Using the shelf

**The herring** (top left; *sill* is Swedish for herring) flips between the sessions and the backend
pages — *Changes*, *Files*, *Agents*, *Settings* (Settings → Default page picks which opens first).
Escape does the same flip; the fish flicks its tail on both.

**Sessions** are tabs. `+` picks the agent (Claude Code, Codex, Gemini, anything installed from the Agents
page) or reopens a stored one. Each session has its own jail, its own conversation in
`~/.local/state/sill/sill.db`, and resumes with history when reopened. Closing a tab stops its jail; the
session stays. The tab's glyph (or a right-click) opens the **session menu**: name, policy, memory scope,
desktop access (X11 / D-Bus — the glyph turns amber while granted) and where the jail runs; a policy or
desktop change rebuilds the jail. The row under the prompt holds only what the next turn uses: mode,
model, context usage, auto-allow (the lock: open = sill answers "allow once" for you), running processes.

**Pins** show as chips above the prompt with their git +/−/untracked counts; a chip opens the file tree
scoped to that folder. Files can be pinned too.

**Changes** (herring → Changes) is every session's pending overlay writes, grouped by session and repo:
diff, Keep, Revert, Keep all, `!` for conflicts. Keep checks that the file is still what you reviewed
and that the host copy hasn't changed since; Keep all takes plain adds and edits only — deletions, type
changes and symlinks take a click each. Nothing is kept automatically.

**Files** is a tree of pinned folders, repositories with changes, and home — git branch on a repo root,
badges for what sessions changed, a search box for names and path fragments. A file opens in the viewer
over the session: read, or *Edit* and Ctrl+S. The tree is a drawer with a dragged width (Ctrl+Shift+F
toggles it).

**Shell**: the terminal icon (or Ctrl+T) swaps the prompt for a terminal running *inside the session's
jail* — same mounts, pins and proxy the agent has, so `git status`, builds and tests show the agent's
view. It starts in the first pinned folder, keeps running while hidden, dies with the session.
Ctrl+Shift+H is a shell on the host instead, marked `host · outside the jail`.

**Permissions and questions.** Permission requests are answered with buttons in the conversation. When
the agent asks you something with choices (Claude's "ask user question", an MCP elicitation) it appears
as a form: pick, or type your own under "Other", and Submit.

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

**Tray icon**: left click shows/hides the shelf, menu has Show/hide and Quit. Quit ends every open session
the way closing its tab does — the agents write their summaries first, out of sight — and exits when the
last one is done. While any agent works, the icon moves (the sill ripples, the fish rides it); it is still
when all are idle. Settings → Tray icon turns it off.

## Memory

Sill remembers sessions on this machine — not *you*, and not the agent's own memory (Claude Code's
`~/.claude` memory files already persist through the shared agent home). `docs/memory.md` is the full
design; this is what you see.

- **Ending a session writes a summary.** Closing a tab with the agent up asks it one last prompt —
  what was asked, what was done and where, decisions, what is unfinished, what not to try again —
  with the facts attached (pins, kept paths, repos and their heads). The tab stays while it answers
  (90 s cap; a second close ends at once with what it has). When the agent can't be asked, sill writes a
  mechanical summary from the facts. Either is stored on the session and marked which kind it is.
- **The Reopen list** (`+`) shows date and the summary's first line under each ended session, the
  whole summary as a tooltip, and a search box over titles, summaries, notes and your prompts
  (`folder:` `agent:` `since:` filters). Clicking a row resumes the session with its history. *New here*
  (`+` on the row) starts a fresh session with the same agent, policy, desktop access, scope and pins,
  told the old one's summary first.
- **The session page** (session menu → *Page*, or the page glyph on a Reopen row) shows the facts
  (agent, when, how long, kept paths, folders), the summary — editable in place, saved as you type
  and marked *edited by the person* — and your **notes**: one line each, Enter adds, × removes;
  they are searched and recalled with the summary. *Summarize now* on an open session asks the agent
  for a checkpoint summary that replaces the text; the one written when the session ends replaces
  that (close the tab mid-checkpoint and the checkpoint becomes the final summary).
- **Recall.** The next session in a folder gets a `[Memory — earlier sessions in these folders …]`
  block in its standing instructions: the last three ended sessions, each with date, duration, agent,
  folders, kept files, commits and the summary. The session menu's **Memory** picks the scope:
  *here* (the same folder, its parents and children, nearest first — the default with pins),
  *everywhere* (the last three anywhere — the default without pins, so a no-pin session is the one
  that sees all of memory), or *off*. Scope decides what is pushed, never what can be searched.
  `memory_recall` in `sill.toml` changes the count.
- **Search from anywhere.** On the host, `sill memory list | show <id|title> | search <words> |
  transcript <id> [--grep WORDS] | recall <id> | refresh`. Inside the jail the agent has
  `sill-memory search | show | transcript` over a read-only snapshot (`/.sill/memory/memory.db`,
  refreshed at shelf start and when a session ends) — it can read memory, never write it. Full-text
  search (SQLite FTS5, no embeddings) over titles, summaries, notes and prompts; agent text through
  `transcript --grep`.
- **Off switch**: Settings → Memory. With it off nothing is summarised or recalled.

Coming: **ghost sessions** — a per-session flag that leaves nothing behind: writes revert after an
hour, no pins, a throwaway agent home, transcript and overlay deleted at close, no memory in or out.

## Agents

**Agents page** (herring → Agents): the ACP registry (`cdn.agentclientprotocol.com`, cached, refreshed
when older than six hours) with search, an All / Installed / Not installed filter and one action per
entry. *Install* puts the package under `~/.local/state/sill/tools/<id>/` (npm with `--ignore-scripts`,
uv with its own tool dir, or the Linux binary with its checksum verified), probes it once over ACP in a
throwaway jail, and writes `~/.config/sill/agents/<id>.toml`, so it appears in the `+` menu with its
icon. The gear on an installed row edits its command, environment (API keys) and proxy domains.
Third-party agents start with an empty allow-list: when one tries a host the proxy refuses, the
conversation asks whether to allow that host (or its domain) for the agent, and the session restarts
with it allowed.

**Agents' own files.** Each agent runs completely inside sill: its paths (`~/.claude` and
`~/.claude.json`, `~/.codex`, `~/.gemini`) are replaced in every jail by a sill-owned copy under
`~/.local/state/sill/agents/<id>/`, seeded once from the host. Other agents' homes are masked from each
jail. A new dot-path an agent creates under `~` is adopted into its sill home after the turn. Caches
(`~/.cache`, `~/.npm`) are per-session directories on the state filesystem, never on the host. Delete
`~/.local/state/sill/agents/<id>` to make the agent log in again.

**Logging in.** When an agent refuses to open a session without a login, the conversation shows one
button per method. Claude Code logs in in the shelf's terminal pane inside the jail — its own login,
separate from the host's Claude Code or Zed. Codex with an API key: put `OPENAI_API_KEY` in its
environment (the gear) and pick *API Key*; with ChatGPT pick *ChatGPT* (or `sill login codex`) — that
login runs on this machine because its browser callback can't reach the jail, but with `CODEX_HOME`
pointing at the jail's copy, so nothing lands on the host. Gemini: see Known limits.

**Policy changes.** A jail gets its mounts and desktop access when it is launched. After editing the
policy (`/etc/sill/policies/default.toml`, then `~/.config/sill/policies/`), reopen a session's tab: a
jail older than the policy file is rebuilt. Restarting the shelf alone does not touch running sessions.

## The command line

```bash
sill toggle                                # show / hide the shelf
sill restart [--dev]                       # after an update (--dev runs the build in target/)
sill theme NAME · sill hotkey SPEC         # the same as the Settings rows
sill memory …                              # see Memory above
sill policy                                # the policy as sill reads it
journalctl --user -u sill-shelf -f         # the shelf's log
```

The jail without the shelf — handy for experiments and scripting. Until the state filesystem is
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

The dev session starts the way the shelf's sessions do (SPEC §4.7): as the transient user unit
`sill-dev.service` where the systemd user instance is reachable (`journalctl --user -u sill-dev`),
otherwise — or with `--direct`, or `SILL_LAUNCHER=direct` — as a detached process with pids and logs
in `$XDG_RUNTIME_DIR/sill/dev/` (the proxy log lists every allow/deny). Direct mode without a delegated
cgroup has no resource limits: the default policy refuses to start then (`[limits] required = true`);
set it to `false` to run anyway with a warning. `--net` shares the host network (testing only).

## Build

```bash
# Debian 13: rustup stable (Debian's 1.85 is too old), bubblewrap, GTK 4
curl -sSf https://sh.rustup.rs | sh
sudo apt install bubblewrap libgtk-4-dev libvte-2.91-gtk4-dev libgtksourceview-5-dev
cargo build --release
cargo test --workspace
packaging/deploy.sh            # from source, in a host terminal (Ctrl+Shift+H): build, sudo sill init, restart, verify
```

Release channels: the tarball (`sill-<ver>-<arch>.tar.gz` + `SHA256SUMS`, `packaging/release.sh`)
with `sudo ./sill init` inside; the `.deb` (`packaging/deb.sh`, `postinst` runs `sill init`); from
source, `cargo install sill-shelf sill-shim` then `sudo sill init` (binaries are taken from next to the
running `sill`). Signed sums, `sill.io` and the apt repository come later.

| Path | What |
|---|---|
| `crates/sill-core` | library: policies, mount table, jail builder (bwrap argv), ledger, keep, memory, shim client, ACP client, proxy |
| `crates/sill-shim` | the supervisor inside the jail: agent stdio relay, control channel, revert, proxy forwarder, `sill-proc`, `sill-memory` |
| `crates/sill-memory` | the FTS5 index, the read-only snapshot and the search queries |
| `crates/sill` | the `sill` command and the GTK shelf (`crates/sill/src/shelf/`) |
| `policies/default.toml` | the built-in policy, also installed to `/etc/sill/policies/` |
| `packaging/systemd/` | state-filesystem units (`/var/lib/sill.img` → `/var/lib/sill`), the shelf's user service |
| `packaging/release.sh`, `packaging/deb.sh` | the release tarball + `SHA256SUMS`, the `.deb`; `packaging/sill.desktop`, `packaging/icons/` |
| `docs/` | `memory.md`, `security.md`, `design.md`, `tasks.md` |

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
- The parked agent process costs ~1.6 % of a core; close the tab when idle until idle-stop exists.
- HTTP CONNECT proxy only (no SOCKS5); programs that ignore `HTTPS_PROXY` have no network, by design.
- Gemini CLI's "Log in with Google" runs host-side like Codex's ChatGPT login, but Google stopped serving
  personal accounts through Gemini CLI on 2026-06-18. Use a Gemini API key (`GEMINI_API_KEY` in its
  environment, then *Gemini API key*), or install Antigravity from the Agents page.
- Sessions end — and are summarised — on a tab close, the idle timeout and the tray's Quit; a shelf restart
  (`sill shelf`) ends none. A tab closed again while its agent writes the summary disappears at once and
  the summary finishes in the background (still capped at 90 s).
- No policies UI yet; edit the TOML.

## Look

Sill's look is its own and the same on every machine: its icons are drawn glyphs (the mark too — a
herring silhouette, `packaging/icons/herring/herring-silhouette.svg`), every theme is a complete
palette (nothing follows the desktop's GTK theme, icon theme or dark/light setting), no libadwaita,
and the text is set in two faces embedded in the binary — **Source Sans 3** for everything
proportional, **Source Code Pro** for terminals, code and the editor (both by Adobe, SIL Open Font
License 1.1; the licence texts ship under `crates/sill/fonts/`). Text size is 13 pt at Normal and
scales with the Text size setting only. `docs/design.md` says why.

## Keys

| Key | What |
|---|---|
| Alt+Space (Settings → Hotkey) | show / hide the shelf |
| Ctrl+Shift+Left / Right | previous / next session tab |
| Ctrl+Shift+T / Ctrl+Shift+W | a new session tab with the same agent / close the tab (writes the summary) |
| Ctrl+Shift+F | the file tree: the session's first pinned folder, else everything; again closes it |
| Ctrl+T | the shell inside the session's jail |
| Ctrl+Shift+H | a shell on the host, in the same pane, marked `host · outside the jail` |
| Escape | the viewer steps back, then back to the sessions |
| F2 | rename the selected tab |
| Enter / Shift+Enter | send / newline |
