# Changelog

What changed in each release of silli. A release's notes on GitHub are its section here
(`packaging/check-release.py --notes`); a version without a section is not released.

## 0.0.6 — 2026-11-17

### Added

- **Subagents**: an agent's subagents (Claude Code, Codex) are shown in the conversation as
  delegation cards with their brief, calls and words, each with its status and elapsed time.
  The subagents at work are listed above the prompt.
- **Always Apply** sessions, switched on in a session's settings: the session's file changes
  are staged for review, and its host control is restricted.
- VM storage is shown by session, and the jail files of ended sessions can be reclaimed.
- Cmd/Ctrl+S saves an agent's instructions from the instructions editor.
- Flush, draggable scrollbars in the source reader.
- In Mark mode, a pen cursor is drawn over the image, anchored at its tip.

### Changed

- A session shows as working while Claude Code or Codex runs a turn it started itself (a
  background task's notification, a goal turn). Stop ends such a turn too.
- Latest in the conversation is a caret-down button that stays legible in every theme.
- The drawer has one edge when it opens over the conversation; the strip before its line is
  gone.
- Session details opens without waiting for the Changes listing.

### Security

- The host's view of a repository reads the parts an agent can write (`.git`, `gitdir:`,
  `commondir`, HEAD, the index, config) without following links or waiting on FIFOs, each
  bounded in size. An untracked link or special file is named in the diff instead of read.
- Ghost and Crosscheck sessions are left out of memory snapshots while copying, so their text
  does not remain in the file's free pages.

### Fixed

- Stop also cancels the subagents still at work; they no longer run on and show the session
  as working again.
- The Changes listing no longer reads ignored package caches in session overlays, which could
  hold the daemon for seconds with many sessions.
- A daemon shutdown could miss a revocation that had just begun and leave its host process
  running; a host job that had just finished could refuse the next one as busy.
- The debug build's window no longer panics at start.
- The daemon names its socket when it cannot make it private.

## 0.0.5 — 2026-11-16

### Added

- **Find (Ctrl+F, ⌘F on the Mac)** in the conversation, in the file reader (Markdown preview,
  diffs and source) and on the Settings page. Matches in folded turns, grouped tool calls and
  tool output are found and opened. Enter and Shift+Enter (or F3) step through them; Aa matches
  case; the shortcut can be changed in Settings → Shortcuts. With a terminal focused, Ctrl+F
  still goes to the terminal. On the Agents page, Ctrl+F goes to its search field.
- **Apply as branch**: a workspace's commits become a new branch in your checkout, which stays
  on its own branch with its files untouched. It sits beside Apply without commit under the
  caret next to Apply.
- Silli.app sets its VM's clock from the Mac at start and after the Mac wakes, so session timers
  and agent sign-ins no longer lag by the length of a sleep.
- A session's launch warnings (a host socket in a pinned folder, missing resource limits) are
  shown in the window: the shield turns yellow and the Permissions panel lists them.

### Changed

- Pinned folders are sent to the agent as ACP additional directories, so an agent that keeps its
  tools to its workspace roots can work in them.
- An agent's settings are in tabs (General, Models, Network, Directories, Advanced). Install and
  Update always take the newest version; the Latest / Supported choice is gone.
- Discarded workspaces are collapsed by default on the Changes page.

### Security

- The seccomp filter also applies on arm64, so jails in Silli.app's VM start with it.
- Git filter drivers from your own git configuration are switched off in the git commands silli
  runs for workspaces, so files are applied and committed byte for byte.
- Host sockets and FIFOs are no longer bound into a jail's layout; one inside a pinned folder is
  named in a launch warning.

### Fixed

- Unreadable directories no longer break the scan of a jail's layout.

## 0.0.4 — 2026-11-15

### Added

- With no session open, the window offers a button per agent that can start one, or Install
  agent.
- Images in the conversation can be viewed full screen.
- An isolation overview on the download page and in the documentation.
- The security audit of 2026-11-15 is published.

### Changed

- The agents marked as tested come from the daemon, so Kimi Code, Mistral Vibe and OpenCode
  show as tested.
- The herring on its silli is the Linux application icon too.

### Security

- The default policy hides npm, PyPI, Docker, Kubernetes, Vault and `pass` credentials
  (`~/.npmrc`, `~/.pypirc`, `~/.docker`, `~/.kube`, `~/.vault-token`, `~/.password-store`).

### Fixed

- Any scroll up in the conversation stops it following new text; scrolling back to the end
  follows again.
- Reused image previews refresh.

## 0.0.3 — 2026-11-15

### Changed

- A new version on `main` is released without a tag push or a draft. (0.0.2 was never
  published; its changes are in this release.)
- Release builds let bubblewrap make user namespaces on the CI runners.

## 0.0.1 — 2026-11-15

The first public release.

- **Jailed by default.** Every session runs Claude Code, OpenCode, Kimi Code or any ACP agent in
  its own bubblewrap sandbox, with no capabilities and its own network namespace. Secrets such
  as `~/.ssh` are hidden, and the network is an allow-list behind silli's proxy.
- **Pins and Changes.** Pinned folders are live; everything else the agent writes waits in an
  overlay for Keep or Revert, with diffs. Workspaces give a session its own copy of a
  repository, applied to your checkout when you choose.
- **Permissions on request.** A test desktop, a private D-Bus or real host operations are
  granted per session and revoked when it ends.
- **Memory.** Each session leaves a summary, so the next agent in the same folder starts
  informed.
- **VM and remote.** The daemon runs on a Linux desktop, in a local Linux VM (Silli.app on Apple
  Silicon Macs) or on another Linux box over SSH.
- Linux packages (tar.gz, deb), a Mac DMG, and an installer script on the download page.
