# Security audit — 2026-11-01

Third pass after SPEC §4.10 and §4.11 (both 2026-10-28). Four code reviews in parallel (jail escape
surface; network egress and agent installs; host-side consumers of agent data; root install and host
command execution), each verifying the earlier claims against the current code, plus a live probe
from inside a running jail. Read-only; nothing here is fixed yet. Findings are ranked by what a
malicious agent gains, not by how easy the fix is.

**Threat model.** (a) The agent inside the jail is fully hostile (prompt-injected): it controls every
byte it writes to the upper trees, pins, its own home, tool-call text, and the network requests it
makes. (b) A second, unprivileged local account on the same machine. (c) The person runs `sudo sill
init` or `sill init --user` from a checkout that an agent could have modified (a pinned folder).

## What holds

Verified in code and, where marked ⊙, confirmed from inside a running jail:

- ⊙ Capabilities empty; user namespaces refused (`--unshare-user --disable-userns`, `jail.rs:242`);
  network namespace with `lo` only; `/run`, `/tmp`, `/proc`, `/dev` fresh.
- ⊙ Secrets masked in both views (`default.toml:40-47`, `jail.rs:363-376`); `/.sill/host` built per
  layer, never recursive — no `/proc`, `/run`, state filesystem (`jail.rs:327-356`).
- ⊙ Shim sockets unreachable: inherited fd, `FD_CLOEXEC` re-armed (`sill-shim/main.rs:154-159`),
  no path, `open()` through `/proc/<pid>/fd` → ENXIO. Runtime dir invisible.
- `--clearenv` + explicit allow-list (`jail.rs:246-279`): no shelf env reaches the agent.
- Units run a **staged** copy of the binary (`session::staged_exe`), unbound in any jail (§4.11 #1).
- Keep: `openat2` `RESOLVE_NO_SYMLINKS|BENEATH` for source and host parent, `O_EXCL|O_NOFOLLOW`
  destination, hash from the bytes copied, setuid/setgid stripped; ledger paths from directory entry
  names only (no traversal); kept-journal in the masked state dir.
- Proxy: suffix match needs a `.` boundary; `evil-github.com`, `github.com.evil.net`, case, trailing
  dot, IDN, port suffix, IPv6 literals all fail closed; loopback/RFC 1918/link-local/CGNAT/ULA and
  mapped v4 refused, shorthand IPs normalised before the check; DNS on the host, every address
  checked, connect to the checked one. No headers logged.
- No Pango markup from agent text anywhere; markdown links are tags without handlers; remote images
  never fetched; SQL fully parameterised; `host_terminal` split with `shell_words`, never a shell;
  `restart` argv is constants; `kill_old_shelves` matches only the user's own `sill shelf`.
- The "session env file" of §4.9 is not read by anything (env is baked into argv at launch).

## Findings

| # | Sev | Finding | Status |
|---|---|---|---|
| S1 | **High** | **Host git against pins runs agent-chosen config, and the agent can trigger it with no click.** Every host-side git call is a bare `Command::new("git") -C <pin>` with no `-c core.fsmonitor=false`, `core.hooksPath`, `--no-ext-diff`, `GIT_CONFIG_NOSYSTEM` or `--no-optional-locks` (`session_view.rs:2030,2037`; `changes.rs:70,139,166,237,270-283,310`). Pins are live rw binds, so `.git/config` is agent-writable. New since 4.11: directory monitors on each pin's `.git` and `.git/refs/heads` (`session_view.rs:1292-1304`, `tree.rs:744-760`) call `refresh_pins → git_summary` on any change there. Exploit: write `core.fsmonitor = /path/in/pin/x.sh`, touch `.git/refs/heads` → the shelf runs the script as the person on the host within a second. `git checkout HEAD --` on revert (`changes.rs:310`) adds the filter/hook path. | **fixed 2026-11-01**: `changes::Git` view (`--git-dir` at a private dir: links to objects/refs/index, HEAD copied, config = format keys only), hooks/fsmonitor off, `DIFF_SAFE`, no writes; revert via `cat-file` + `safe_fs::write_file_beneath`. Test: `git_view_runs_nothing_from_the_repository_config`. |
| S2 | **High** | **Binds are mounted after masks, and sill's own config is keepable.** Order in `jail::plan`: overlays → host view → masks (`jail.rs:362-371`) → direct/ro → `self.binds` (`:389-390`: agent home, `shared_files`, pins, private). Nothing refuses a bind under or above a mask. `~/.config/sill/agents/<id>.toml` and `policies/*.toml` are ordinary overlaid files and Keep does not exclude them (`changes.rs` has no sill-config exclusion). Exploit: the agent writes `shared_files = ["~/.ssh/id_rsa"]` (or a pin of `~`, which mounts real home over the `~/.ssh` tmpfs) into the override; it shows as one "M" line among the turn's edits; after Keep and the next start the secret is a rw bind inside the jail. Same root cause as §4.11 #6, parent-pin variant new. | **fixed 2026-11-01**: `jail::no_bind_across_mask` refuses a bind under/over a mask; built-in masks for `~/.config/sill`, `~/.local/state/sill/bin`, `sill.db*`; `keep::protected` refuses sill's config/state and `.git/config`/`.git/hooks` (Keep all skips, button disabled with the reason). `expected_hash` (S10) still unwired. Refined the same day: the rule as written also refused sill's *own* trees — the session's private `~/.cache` scratch over a masked `~/.cache/<other agent's adopted path>` failed every jail on a machine with one. A bind whose source is not the host path (private scratch, the agent's sill-owned home) exposes nothing of the host beneath it, so over a mask it is mounted and the mask laid again on top (the path stays hidden inside the bound tree); inside a mask it is still refused (the copy is seeded from the host). The host's own tree (pins, `shared_files`, `direct`, `ro`) is refused both ways as before. |
| S3 | **High** | **Installing an agent runs the downloaded package on the host.** `registry.rs:259` → `probe_load_session` (`:512-521`) spawns the package's command on the host as the user, full shelf env, cwd `$HOME`, 20 s deadline. `npm --ignore-scripts` and the tooltip "no install scripts run" (`agents_page.rs:431`) are true but moot: the `bin` runs anyway. `uv tool install` builds sdists on the host; `binary.sha256` is optional (`:471`). One click on an "untested" registry entry = arbitrary host code with the user's credentials. | **fixed 2026-11-01**: `probe_load_session` runs in a throwaway bwrap jail (no net, no caps, `/` ro, home tmpfs but for the tools dir and the runtime's dir, state fs hidden); `binary.sha256` mandatory. Open: `uv tool install` still builds sdists on the host; index unsigned (S13). |
| S4 | **High** | **Host-side login runs the agent on the host with a directory the jailed agent writes freely.** `login.rs:36-45,78-79` runs `agent.command` on the host with `CODEX_HOME`/`GEMINI_CLI_HOME` (or `$HOME` for third-party agents, `:55-76`) pointed at `~/.local/state/sill/agents/<id>/…`, which is bound **rw** into every jail of that agent (`session.rs:192-200`, `jail.rs:389`). The agent decides when "Login needed" appears (`auth_required`). Exploit: jailed Gemini writes `.gemini/settings.json` with an `mcpServers.x.command`, returns `auth_required`; the person clicks "Log in" → gemini on the host spawns the command. No Keep involved. | **fixed 2026-11-01**: `login::run` relocates the home to `~/.local/state/sill/login/<id>` made fresh for the login; on success its files are copied into the jail's copy with `safe_fs::copy_files_beneath` (no links followed), then removed. |
| S5 | High (b) | **`sudo sill init` chowns whatever sits at `/var/lib/sill/<uid>`.** The parent is 1777; `init.rs:349-356` accepts an existing dir, then `set_permissions(0700)` + `chown` (both follow symlinks, neither checks the owner). Runs on every `sill init` and every package upgrade (`deb.sh:126`). A second local user pre-creates `/var/lib/sill/1000/sessions/` 0777: the victim's overlays are attacker-writable (content appears in the jail and in Changes; Keep copies it to the host). With `fs.protected_symlinks=0` a symlink there makes root chown `/etc`. `session::state_dir` likewise never verifies ownership. | **fixed 2026-11-01**: `init` creates the per-uid dir itself or opens it `O_NOFOLLOW` and refuses one owned by anyone but root or the user; `session::state_dir` uses `safe_fs::owned_dir` (owner must be the caller) for `<uid>` and `<uid>/sessions`. |
| S6 | Med-High | **Attachment copy follows agent-controlled symlinks on the host.** `attach_path` (`session_view.rs:1554`) does `create_dir_all` + `fs::write` into `<state>/private/<cache>/sill/uploads/<name>`, a directory bound rw into the jail; the agent can replace `uploads`, or the file name, with a symlink (or a FIFO, which blocks the GTK thread). The bytes and name are the person's, so it needs steering, but the agent authors files in pins. | **fixed 2026-11-01**: `attach_path` writes through `safe_fs::write_file_beneath` from the session's state dir (`RESOLVE_BENEATH|NO_SYMLINKS`, `O_NONBLOCK` against a FIFO, regular file checked). |
| S7 | Medium | **"Allow host" prompt built from an unvalidated agent string.** Any `deny` line, resolve failures included, puts the raw CONNECT authority into `denied` (`main.rs:621-627`); `session_view.rs:1452-1480` shows it verbatim and offers `Allow <host>` and `Allow *.<labels[1..]>`. `CONNECT *:443` → an "Allow *" button (`*` matches everything, `proxy.rs:46`); `evil.co.uk` → "Allow *.co.uk"; confusables and bidi controls pass; unlimited distinct hosts flood the conversation. Human click only; the text shown is what is saved. | **fixed 2026-11-01**: `proxy::valid_hostname` (IP literal or ASCII DNS labels — no wildcard, no bidi/confusables) gates the launcher's `denied` file and, again, the shelf's reading of it; the wildcard offer comes from `proxy::wildcard_offer` — three labels or more, and never `*.<public suffix>` (a short list: `co.uk`, `github.io`, `amazonaws.com` …); after 8 refused hosts per session start the conversation says so once and the rest stay in the log. Tests: `names_the_shelf_may_show`. |
| S8 | Medium | **Host shell / host terminal open in an agent-writable folder by default.** `open_host_terminal` (`window.rs:596-600`) and `show_host_shell` (`session_view.rs:633-646`) take cwd = tree selection, else the first pinned folder, and spawn `$SHELL -l` there. Git-aware prompts (`__git_ps1` with dirty-state, oh-my-zsh git plugin) run `git status` and honour `core.fsmonitor`/`core.hooksPath` from the pin's `.git/config` at the first prompt — S1 through the person's own shell. | **fixed 2026-11-01**: both host shells (the pane's `$SHELL -l`, the terminal window) get `changes::HOST_SHELL_GIT_ENV` — `GIT_CONFIG_PARAMETERS` turning `core.fsmonitor` and the builtin monitor off for every repository, so a prompt's `git status` runs nothing from the pin's config. Hooks, filters and pagers stay: they run only for git commands the person types. Test: `launch_terminal_runs_in_the_folder`. cwd stays the pin (the point of the shell). |
| S9 | Medium | **Viewer and tree follow symlinks in pins; save writes the target.** `viewer.rs:464/478/488` use `is_file`/`read`, `save()` at `:393` is `fs::write(&path)`; `tree.rs:55/235/304` descend symlinked dirs. A symlink dropped in a pin is shown under the pin's name; Edit + Ctrl+S writes the link's target anywhere on the host, nothing in the title says so. | **fixed 2026-11-01**: save via `safe_fs::replace_file` (a link at the name refused); the tree lists with `file_type()` (no follow), descends only a real directory and shows a link as a link glyph with its target in the tooltip; the viewer refuses to show a link under the pin's name and says where it points. Open: a link in a path *component* above the file (a linked parent directory) is not re-checked by the viewer — the tree never lists such a path, so only a chip or a pasted path reaches it. |
| S10 | Medium | **Keep: `expected_hash` never passed, "Keep all" unfiltered.** `shelf/changes.rs:392,467` build `KeepOptions { undo, ..Default::default() }`; `keep.rs:277` checks only when `Some`. Keep all skips only conflicts/ignored: deletions, type changes, symlinks, `.git/config`, hooks, `~/.config/sill/*` go in one click, agent not paused. Undo makes it reversible, not safe. | **fixed 2026-11-01**: the Changes scan hashes each listed regular file's upper content (`Scanned::hashes`) and every Keep passes it as `expected_hash`, so a swap between the review and the copy is refused; Keep all keeps only added/modified files and directories (`keeps_in_bulk`) — deletions, type changes, replaced folders and symlinks need a click each, and the result line says how many were left. Agent not paused (open). |
| S11 | Medium | **Shim `revert` follows symlinks at the destination.** `sill-shim/main.rs:588-625`: host side resolved through `/.sill/host`, then `fs::copy`/`create_dir_all`/`set_permissions` on `path` in the agent-controlled upper tree. Reach limited to `direct`/pin targets (already writable), rises with configured `direct` trees. | **fixed 2026-11-01** (§4.11 #10): parent opened `RESOLVE_NO_SYMLINKS`, every step relative to it. |
| S12 | Medium | **`init --user` / `init` bake `current_exe()` from a checkout into autostart; `--prefix` unvalidated.** `init.rs:526,548-553` write `ExecStart=<current_exe> shelf` into the user unit / `.desktop`; run from `target/debug/sill` in a pinned checkout the agent has persistence on every login, no warning (unlike `restart --dev`). `init.rs:479-500` boot hook runs `<prefix>/bin/sill`; `--prefix` may be relative or user-writable. | **fixed 2026-11-02**: `init --user` copies the binary and `sill-shim` to `~/.local/state/sill/bin/autostart/` (masked in every jail, replaced by rename) and the user unit / `.desktop` run that copy; `init` refuses a `--prefix` that is relative or has a directory above it not root-owned or group/other-writable (`init::check_prefix`). |
| S13 | Medium | **Registry index unsigned, unpinned.** `registry.rs:19,184-201`: curl, no signature, no version pin, `--no-package-lock`, optional sha256. Cache is in private `~/.cache` by default (not agent-writable) but a policy without that entry makes it keepable. | **fixed 2026-11-02**: the registry is a snapshot compiled into sill (`crates/sill-core/registry/snapshot.json`: every `agent.json` of `agentclientprotocol/registry` at one commit, quarantined ids dropped, a full `package-lock.json` per npm package resolved with `--package-lock-only`), rebuilt by `sill registry snapshot` and reviewed as a diff; snapshot entries install with `npm ci --ignore-scripts` against that lockfile and the installed version is checked, `uv tool install --no-build` with an `==` pin (no sdist builds on the host), binaries by sha256 as before. The live index still lists newer or unknown entries, marked *unverified*: installing one needs a second click on the row, pins the index's exact version and records `trust = "live"` in `installed.json`. The live index and icons moved from `~/.cache` to `~/.local/state/sill/registry/`, masked in every jail like the agents' homes. |
| S14 | Medium | **"Host-local" refusal is address-class based.** `proxy.rs:55-75` misses the host's own global addresses, LAN neighbours with public/global-v6 addresses, `0.0.0.0/8`, `192.0.0.0/24`, `198.18/15`, `240/4`, `64:ff9b::/96`, `2002::/16`. Only reachable under `mode = "all"`; the default allow-list is unaffected. | **partly fixed 2026-11-01**: the reserved blocks, NAT64 and 6to4 are refused (`proxy::is_local`); the host's own global addresses and LAN neighbours with global addresses are not (would need the host's interface list). |
| S15 | Low | **User journal readable from inside the jail.** `/var/log` is an ordinary overlaid dir; `/var/log/journal/<mid>/user-<uid>.journal*` is user-readable by ACL → every session's `allow/deny host:port` lines, other sessions' agent stderr, every user service's log. | **fixed 2026-11-02**: `/var/log/journal` is a built-in mask (`jail::plan`, with sill's own files) |
| S16 | Low | **Session ids never validated at the sinks.** `state_dir`, `remove_state` (`unshare -Ur rm -rf <sessions/<id>>`), `unit_name`, `runtime_dir` join the id unchecked. Ids only come from `new_id()` today, but `sill.db` lives under `~/.local/state/sill`, overlaid and not masked, so a crafted db reaches the host through Keep (S2/S10 amplifier). | **fixed 2026-11-01**: `session::valid_id` (1–64 of `[A-Za-z0-9_-]`) at `state_dir` (so `remove_state` too) and `shim::runtime_dir`; `unit_name` reduces anything else to `_`. Test: `ids_at_the_sinks`. |
| S17 | Low | **VTE Ctrl+click opens `https?://` matches straight into `UriLauncher`** (`session_view.rs:553-567`), no click-through as §4.5 promises; jail shell output is agent text. Images: any host file with an image extension (SVG included) is decoded by gdk-pixbuf/librsvg on the host (`session_view.rs:1641-1684`). | new |
| S18 | Low | **Transient `sill-<id>` units and `sill-shelf.service` have no systemd hardening** (no `NoNewPrivileges`, `ProtectSystem`, seccomp). bwrap is the boundary; a bwrap bypass has no second fence. | **partly fixed 2026-11-02**: a seccomp deny-list (`sill-core::seccomp`, classic BPF built by hand, no libseccomp) goes to bwrap as `--seccomp <memfd>` in every session jail, `sill jail run` and the install probe — mounts and the new mount API, modules, kexec, keyrings, bpf, userfaultfd, perf, io_uring, fanotify, `setns`, `clone`/`unshare` with namespace flags (kept when `jail.allow_userns`), `clone3` → ENOSYS (libc falls back to `clone`), the x32 ABI → ENOSYS, a foreign arch → kill; ptrace stays for strace/gdb. x86_64 only (another arch: a warning, no filter). Policy `jail.seccomp = false` turns it off. Session units get `NoNewPrivileges=yes`. Open: `ProtectSystem`/`RestrictNamespaces` would break bwrap in the unit; Landlock needs a PID 1 in the jail; the shelf unit untouched (it spawns the person's shells). Test: `seccomp::decides_as_documented` (a BPF interpreter over the program), `jail::seccomp_flag_follows_policy_and_fd`; probed live 2026-11-02 with `prctl(PR_SET_SECCOMP)` on the written program: keyctl/bpf/io_uring/`unshare(CLONE_NEWNS)` EPERM, `unshare(CLONE_FILES)`, fork and spawn fine. |
| S19 | Low | Minor: `--state-dir` fstab line lacks `nosuid,nodev` (`init.rs:310`; **fixed 2026-11-02**); image briefly 0644 before chmod; proxy `serve` ends on the first `accept` error and `addrs[0]` panics the handler on an empty resolve (`proxy.rs:105,171`); `Proxy-Authorization` forwarded upstream (`:182`); any port on allowed hosts (§4.11 #9). | proxy items **fixed 2026-11-01** (§4.11 #9); fstab/image mode open |
| S20 | Info | **SPEC §4.11 claims "Host-mismatched absolute-form refused" — the code forwards `Host:` verbatim** (`proxy.rs:177-188`); the decision uses the request-line authority only. Not a bypass (the TCP target is the checked one); the SPEC should say "ignored". Raw host bus grants were removed on 2026-11-06. Default allow-list permits exfiltration via GitHub/npm by design. | doc / known |
| S21 | Low | **State files take their mode from the umask, not from sill.** `store::open` lets SQLite create `sill.db` (SQLite's default 0644 & ~umask), `create_dir_all` makes `~/.local/state/sill` and `sessions/` with 0777 & ~umask, `shelf.log` likewise; on this box (umask 002, user-private group) they are 0775/0664, on a machine with a shared `users` group or umask 022 they are group- or world-readable. The agent homes and the `kept/` journal are already explicit 0700/0600 (`session.rs:220-231`, `keep.rs:253`). Same-uid readers are unaffected either way (see Data at rest). | **fixed 2026-11-01**: `store::open` makes the dir 0700 and sets 0600 on the db (and `-wal`/`-shm` when present; SQLite copies the main file's mode for new ones), `shelf.log` is opened 0600 |

## Status

2026-11-01, same day: S1–S6 fixed (status column; commit after this audit). New module
`sill-core::safe_fs` carries the symlink-proof write/remove/copy and the owner check; `changes::Git`
is the only way the shelf runs git. Later the same day S8 (host shells with git's fsmonitor off)
and S10 (`expected_hash` wired, Keep all limited to plain file writes) followed, then S7 (the
Allow-host prompt from a validated name, no public-suffix wildcards, capped), S9 (tree and viewer
stop at links) and S16 (session ids checked at the sinks). Still open from this pass: S12, S13,
S15, S17–S21.

2026-11-01, later: the §4.11 leftovers closed in seven commits — #8 (other agents' credentials
masked, homes 0700), #7/#9 (proxy decides on the name before DNS, one port per pattern, connection
cap, head cap, accept survives; S14's reserved ranges, S19's proxy items), #10 (shim revert and
diffs never through a link; S11, S9's save half via `safe_fs::replace_file`), #16 (keep re-stats the
host), #13 (session start, git status, keep/revert off the GTK thread), #15 (weak closures in the
session view), #17 (store busy timeout and atomic seq, ledger EACCES, hotkey HUP, store-open error
window). Open from this file: S12, S13, S15, S17, S18, S19's
fstab/image items.

2026-11-02: S18's jail half (seccomp under bwrap, `NoNewPrivileges` on session units), S15 and S19's
fstab line closed — the comparison below's items 1 and 2 but for S12. Open: S12, S13, S17, S19's
image mode, S18's systemd/Landlock remainder, S10's pause.

## Data at rest

The question came up (2026-11-01): nothing sill stores is encrypted — transcripts, summaries, the
upper trees, the kept journal, the agents' own homes with their credentials — all plain files owned
by the user. That is the stance, and the reasoning is by reader:

| Reader | What protects the data | Would a sill cipher add anything |
|---|---|---|
| Another local user | 0700 per-uid dirs (S21 makes it explicit) | no |
| Someone with the disk (theft, a discarded drive) | **full-disk encryption**, the OS's | no: a key sill can read at shelf start sits under the same uid as the data; the thief has both. Only a passphrase typed at every start would differ, and the agents' homes (`~/.claude.json` credentials, their own transcripts, plaintext as the vendors ship them) would stay in the clear anyway |
| Another process running as the user on the host | nothing — it reads a key the way sill would | no; this is the jail's threat in the other direction |
| **The agent in the jail** | masking: `~/.config/sill` and the whole of `~/.local/state/sill` (`sill.db*`, `bin`, the agents' homes, the daemon's lock, the shelf's log) are tmpfs in every jail, only `tools/` bound back read-only (S2, S16, §4.11 #8; whole-directory since 2026-11-02) | no — and this is the reader that matters |

So an application-level layer would protect against none of the four and suggest protection against
the second. What does help, and what sill does or should do:

- **Recommend FDE as the baseline**, in the install docs: sill's data at rest is as safe as the disk.
- **fscrypt on the state image** for whoever wants a per-user key: `/var/lib/sill.img` is ext4, which
  supports fscrypt natively, and `pam_fscrypt` unlocks a directory with the login passphrase. The
  upper trees are the richest target — copy-up puts a copy of *every* file the agent touched there,
  `~/Documents` or a `.env` included — and this covers them with no sill code: a documented
  `fscrypt encrypt /var/lib/sill/<uid>` step, maybe a `sill init --fscrypt` later.
- **Hygiene over secrecy**: auto-revert, deleting the upper tree at close, ghost sessions'
  `secure_delete` + `VACUUM` (docs/memory.md §3.4). Transcripts hold whatever the agent `cat`ed; a
  redaction regex on store would miss most of it and promise more than it does — not planned.
- **Explicit modes** (S21): 0700 on the state dir, 0600 on the database and log, independent of the
  umask. The `kept/<id>/prior` payloads keep the host file's own mode, under a 0700 parent, which is
  right: undo must put the mode back.

## Proposed top three (as written before the fixes)

Chosen by reach × likelihood under threat (a), the agent that is already running. S1, S2 and S4 share
one shape — *the person's host-side tooling consumes a tree the agent writes* — and each is a
complete host compromise. S3 and S5 are as severe but need a different actor (a malicious package,
a second local user) and can follow.

**1. Stop running host git against anything the agent can write (S1, S8).** Short term, every
host-side invocation gets `-c core.fsmonitor=false -c core.hooksPath=/dev/null -c diff.external=
-c filter.*.clean= --no-ext-diff --no-optional-locks` plus `GIT_CONFIG_NOSYSTEM=1` and
`GIT_CONFIG_GLOBAL=/dev/null`, through one `fn git(repo) -> Command` in `sill-core::changes` that
every caller must use (`session_view::git_summary`, `tree`, `viewer`, `changes`); `git checkout` for
revert on a pin is replaced by the shim doing it inside the jail. The host shell defaults to `$HOME`,
not the pin, and exports the same `GIT_CONFIG_PARAMETERS` so prompts are safe too. Long term (SPEC
§4.11 #3's real fix): run the queries through the shim in the jail, or `gix`, and the `-c` list
becomes defence in depth. Config keys are a moving target; the in-jail route is the one that stays
fixed.

**2. Make the mount plan refuse a bind that touches a mask, and keep sill's config out of the
agent's reach (S2, S16, S10's worst case).** In `jail::plan`, before emitting `self.binds`, `direct`
and `ro`, reject (fail the session start with a note naming the path) any bind whose path is under
or an ancestor of a masked path — a pin of `~` and `shared_files = ["~/.ssh/id_rsa"]` both die
there, whatever the config says. Then take `~/.config/sill` and `~/.local/state/sill` out of the
overlay (mask them, like the state dir), and have Keep refuse them and `.git/config`/`.git/hooks`
under any path with a separate confirmation. The `expected_hash` wiring (S10) fits the same change:
the hash the diff showed goes into `KeepOptions`.

**3. Never run an agent on the host from a directory the jail writes (S4, S3).** The host login
runs from a pristine, shelf-owned copy of the agent home: copy out only the credential files named
in `shared_files`/`home_paths`, point `CODEX_HOME`/`GEMINI_CLI_HOME`/`$HOME` at a fresh temp dir
seeded with nothing else, copy the login file back when it exits. The install probe
(`probe_load_session`) runs inside a throwaway jail (`--unshare-all`, no home, no network) or is
dropped in favour of reading `package.json`/the index's capability field; `binary.sha256` becomes
mandatory and npm specs are pinned exactly. Both are the same rule: the agent's binaries and its
writable config never meet on the host.

Then, in order: S5 (`O_DIRECTORY|O_NOFOLLOW` + owner check in `init` and `state_dir`), S6/S9/S11
(one `openat2`-beneath helper shared by uploads, viewer save, revert), S7 (LDH-only hostnames, no
`*`, no public-suffix wildcards, a per-session cap), S12, S13, S14, S15 (mask `/var/log/journal`),
S17, S20 (SPEC wording).

## Compared with other agent sandboxes — 2026-11-02

Against what Zed, Claude Code and Codex CLI document for their own sandboxes (Zed blog
"Sandboxing" and docs `ai/sandboxing`, 2026-08; Claude Code's bash sandbox; Codex CLI's
Seatbelt / bwrap + Landlock), read next to this file and the README.

| | sill | Zed | Claude Code | Codex CLI |
|---|---|---|---|---|
| What is inside the boundary | **the whole agent process** — binary, shell, MCP servers, everything it spawns, one bwrap jail per session | the native agent's **terminal tool** only; edit/fetch run in the editor under permission prompts; external ACP agents (Claude Code, Gemini CLI) not sandboxed by Zed | the **Bash tool** only; Read/Edit/Write in the unsandboxed parent | every command, Seatbelt or bwrap + Landlock; read-only default |
| Filesystem | overlay: the agent writes anywhere, unpinned writes stay on the state fs until Keep; pins are live rw binds | deny-list: no writes outside the project, none to `.git` | allow/deny paths; falls back to unsandboxed by default when the sandbox cannot start | workspace-write or read-only |
| Network | per-agent allow-list through sill's proxy, refused host → Allow button, DNS on the host, local ranges refused | blocked for sandboxed commands | proxy allow-list | off |
| Secrets | masked in every jail (`~/.ssh`, keyrings, `.netrc`, cloud creds, other agents' homes); the agent's own login is a sill-owned copy | the project-dir rule only | paths only | — |
| Review | Changes view, per-file Keep/revert with hash check and undo journal | editor diff for edit-tool changes | git | git |
| Platforms | Linux, bwrap; a session that cannot build its jail does not start | macOS, Linux, WSL; runs unsandboxed with a warning when bwrap is missing | macOS, Linux | macOS, Linux, WSL |

**Where sill is ahead.** The boundary is around the agent, not around one tool: a prompt-injected
agent abusing *any* tool, MCP server or subprocess is still in the jail — Codex's model, applied to
arbitrary ACP agents. Overlay + Keep lets the agent install tools and touch `~/.config` and leaves
the decision to the person afterwards, where a deny-list just stops it. The host-side consumer
surface is audited (S1–S11 above): Zed names the `.git` hooks problem and forbids writes to `.git`;
sill runs host git with a private `--git-dir` and filtered config, gives host shells
`GIT_CONFIG_PARAMETERS`, has Keep refuse `.git/config`, `.git/hooks` and sill's own config, never
runs an agent on the host from a jail-writable directory, probes installs in a throwaway jail.
Memory and transcripts are read-only snapshots inside the jail.

**Where sill is behind, or open.**

- **One fence** (S18). bwrap is the only boundary — no seccomp filter, no Landlock, no systemd
  hardening on the units. Codex layers Landlock under bwrap. A bwrap or kernel bug is a full escape.
- **Linux-only is by design, not a gap to close in the jail code**: on macOS and Windows sill will
  run a Linux VM (real virtualization) with sill-linux inside it, so the jail stays one
  implementation and the VM is the second fence there. Zed's "run unsandboxed with a warning"
  fallback is not wanted: a session whose jail cannot be built must not start (as today).
- **Exfiltration through the default allow-list** (S20): github.com and npm are allowed by design.
  Same for everyone; worth saying in the install docs.
- **Open findings**: S12 (autostart bakes `current_exe` from a checkout), S13 (unsigned registry
  index), S15 (user journal readable in the jail), S17 (VTE link click-through, host-side image
  decoding), S19 (`nosuid,nodev` on the state fstab line), S10's remainder (agent not paused
  during Keep).
- **Desktop access:** raw host X11 / D-Bus grants were removed on 2026-11-06; managed
  Weston/private Xwayland is planned, with no desktop backend available yet.

**Order proposed.**

1. **A second fence in the jail (S18)**: a seccomp BPF filter passed to bwrap with `--seccomp <fd>`,
   the container-standard deny list (`mount`, `umount2`, `pivot_root`, `ptrace` of other pids,
   `keyctl`, `bpf`, `userfaultfd`, `perf_event_open`, `io_uring_*`, `kexec_*`, `open_by_handle_at`,
   `unshare`/`setns` with namespace flags, `add_key`, `request_key`) — the escape surface of nearly
   every container CVE of the last years, and an hour of code against bwrap's existing flag.
   Then `NoNewPrivileges=`, `ProtectSystem=strict`, `RestrictNamespaces=` on `sill-<id>` units
   and `sill-shelf.service` as the layer outside bwrap. Landlock for the ro-policy paths later
   (needs a PID 1 in the jail to apply it; the shim is the candidate).
2. **The small ones in the same pass**: S15 (mask `/var/log/journal`), S19 (`nosuid,nodev`, image
   mode), S12 (refuse to bake a `current_exe` under a pin or a writable checkout into autostart —
   point the unit at the installed `<prefix>/bin/sill` or refuse with the reason).
3. **S13, before the registry is public**: a signed index (minisign, key in the binary), exact
   npm specs with integrity hashes, sdists refused for `uv tool install`.
4. **Pause the agent during Keep** (S10 remainder): the shim holds the agent's turn (or the shelf
   waits for idle) while the copy runs, closing the window the hash check only narrows.
5. S17 (link click-through, decode images in a jail or via a size-capped loader).
