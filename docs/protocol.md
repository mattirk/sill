# sill — the daemon protocol

Status: protocol **5**, frozen 2026-11-02 (docs/daemon.md M3). From here every change
to a method or a type's shape is a line in the changelog below and a bump of
`sill_proto::PROTOCOL`; a client whose number differs from the daemon's asks it to leave and
connects to (or starts) the installed binary, so the two are never mismatched for long.

`sill daemon schema` prints the whole protocol as one JSON document — this table, each method's
params and result as `$ref`s, and the JSON Schema (draft 2020-12) of every type under `$defs` —
for a frontend in another language to generate from. The Rust side is `crates/sill-proto`
(`sill_proto::wire` for the types, `sill_proto::Client` for a blocking client); it links no jail,
store or GTK code. This file is the human reading of the same thing; the table below is generated
from the schema output and a test (`sill_proto::schema::tests`) checks that every method the
daemon answers is named here.

## 1. Transport

- **Socket**: `$XDG_RUNTIME_DIR/sill/daemon.sock`, mode 0600 in a 0700 directory; the daemon
  refuses a peer whose uid is not its own (`SO_PEERCRED`). A frontend that finds no socket starts
  the daemon (`Client::connect_or_start`): the systemd socket unit where installed, else a
  transient `sill-daemon` user unit, else a bare process (docs/daemon.md §3.1).
- **Framing**: JSON-RPC 2.0, one message per line (`\n`), UTF-8. A request carries `id`; its
  answer comes with the same `id`, in order of arrival on that connection. A notification has no
  `id` and no answer. Errors use JSON-RPC's `error {code, message}`; the codes are
  `sill_proto::rpc::Error`'s (parse error, invalid params, method not found, and a generic failure
  whose `message` says what).
- **Method names** are ACP extension methods, `_sill/<group>.<name>`, so a connection could carry
  ACP's own `session/*` traffic one day.
- **One connection, one subscriber set**: `sessions.attach` subscribes the connection to a
  session's `session.event` notifications (the session's current state first — capabilities,
  controls, a waiting login, the turn in flight with its text so far, unanswered questions);
  `sessions.detach`, a `Quit` command or the connection closing unsubscribes. The daemon's queue
  per connection is bounded; a frontend that stops reading is cut off and attaches again.
- **Questions**: a permission request or a form arrives as an event with a `request` number and
  waits — with nobody attached too — until some frontend answers it with `sessions.answer`; the
  first answer counts and every attached frontend then gets `Resolved {request}`.
- **Timeouts**: the Rust client waits 120 s for an answer (5 s for `daemon.status` and
  `daemon.shutdown`) and then reports the daemon stuck (docs/daemon.md §3.7).

## 2. Methods

| Method | Kind | Params | Result | What |
|---|---|---|---|---|
| `_sill/daemon.status` | request | `Empty` | `DaemonStatus` | Version, protocol, pid, mode and session counts of the daemon. |
| `_sill/daemon.shutdown` | request | `Empty` | `{}` | Ask the daemon to leave; sessions keep running and the next daemon adopts them. |
| `_sill/sessions.list` | request | `Empty` | `[Session]` | Every session in the store, newest first. |
| `_sill/sessions.get` | request | `Key` | `Session` | One session by id, id prefix or exact title. |
| `_sill/sessions.new` | request | `NewSession` | `Key` | Create a session row; answers its id. |
| `_sill/sessions.set` | request | `SessionSet` | `{}` | Change one field of a session's row. |
| `_sill/sessions.delete` | request | `Key` | `{}` | Stop the jail, remove the session's state and its row. |
| `_sill/sessions.pin` | request | `Pin` | `{}` | Pin a path in a session (bound at its next start). |
| `_sill/sessions.unpin` | request | `Pin` | `{}` | Unpin a path. |
| `_sill/sessions.start` | request | `Key` | `Started` | Start a session's jail, or find it running. |
| `_sill/sessions.stop` | request | `Key` | `{}` | Stop a session's jail; the row stays. |
| `_sill/sessions.log` | request | `LogRequest` | `Text` | The tail of a session's log. |
| `_sill/sessions.attach` | request | `Attach` | `{}` | Start jail and agent when needed and subscribe this connection to the session's events; its current state comes first. |
| `_sill/sessions.detach` | request | `Key` | `{}` | Unsubscribe; the agent stays the daemon's. |
| `_sill/sessions.command` | request | `SessionCommand` | `{}` | A prompt, cancel, mode, option, login or quit for a running session; a prompt during a turn is refused. |
| `_sill/sessions.answer` | request | `SessionAnswer` | `{}` | The answer to an agent's question (permission or form); the first answer counts. |
| `_sill/session.event` | notification | `SessionEvent` | — | One event of a session, to every connection attached to it. |
| `_sill/sessions.end` | request | `End` | `Ending` | End a session as closing its tab would: the summary turn when the agent can be asked, the row written or deleted, the jail stopped. |
| `_sill/sessions.checkpoint` | request | `Key` | `{}` | Summarize now, without ending; the summary comes as a Summarized event. |
| `_sill/sessions.changes` | request | `Key` | `Count` | How many unkept writes a session has outside its pins. |
| `_sill/sessions.jail_view` | request | `Key` | `JailView` | A session's jail layout, for showing files as the agent sees them. |
| `_sill/pins.list` | request | `Empty` | `[path]` | The global pins (docs/pins.md). |
| `_sill/pins.add` | request | `GlobalPin` | `{}` | Add a global pin (canonicalized; refused inside another or in sill's state). |
| `_sill/pins.remove` | request | `GlobalPin` | `{}` | Remove a global pin. |
| `_sill/changes.list` | request | `Empty` | `ChangesList` | Every session's overlay changes and undoable keeps, and the pinned repositories' git status. |
| `_sill/changes.diff` | request | `DiffOf` | `Text` | One change's diff. |
| `_sill/changes.keep` | request | `ChangesKeep` | `Done` | Copy changed paths to the host; each checked against the hash it was listed with. |
| `_sill/changes.revert` | request | `ChangesPaths` | `Done` | Put the host's version back in the session. |
| `_sill/changes.undo` | request | `ChangesUndo` | `Done` | Undo keeps. |
| `_sill/changes.git_revert` | request | `GitRevert` | `Text` | A pinned repository's path back to HEAD. |
| `_sill/changes.history` | request | any | any | The ledger across time (SPEC §3.6); params and result are sill_core::history's types, not in this schema yet. |
| `_sill/changes.changed` | notification | `Empty` | — | The overlays changed (a sweep, a keep, a turn, a deletion): whatever shows them rescans. |
| `_sill/memory.list` | request | `Empty` | `[MemorySession]` | Ended sessions, newest first. |
| `_sill/memory.show` | request | `Key` | `Text` | An ended session's summary and notes, as the CLI prints them. |
| `_sill/memory.search` | request | `MemorySearch` | `Text` | Full-text search over summaries and transcripts. |
| `_sill/memory.transcript` | request | `Transcript` | `Text` | A session's transcript. |
| `_sill/memory.note_add` | request | `Note` | `{}` | Add a note to a session's memory. |
| `_sill/memory.note_remove` | request | `Note` | `{}` | Remove a note by its time. |
| `_sill/memory.compact` | request | `Empty` | `Compacted` | Compaction and retention now. |
| `_sill/memory.forget` | request | `Empty` | `Count` | Delete every ended session. |
| `_sill/agents.set_settings` | request | `AgentSettingsSet` | `{}` | An agent type's default settings. |
| `_sill/settings.get` | request | `Empty` | any | sill's settings (sill.toml); the type is sill_core::config's, not in this schema yet. |

`Empty` is `{}`; `[path]` is a JSON array of path strings; "any" is a shape not in the schema
yet (`changes.history` and `settings.get` carry `sill_core`'s types; only the shelf reads them).

## 3. Events

`session.event {key, event}`: `key` is the session id, `event` one of (`sill_proto::wire::Event`,
externally tagged — `{"Text": "…"}`, `{"ToolCall": {…}}`, `"TurnStarted"`):

- the conversation: `Ready`, `Capabilities`, `Controls`, `TurnStarted`, `UserText`, `Text`,
  `Thought`, `ToolCall`, `ToolCallUpdate`, `SubagentText`, `Usage`, `Stopped`, `Ended`;
- questions: `PermissionRequest`, `Elicitation` (a form), `AutoAnswered`, `Resolved`;
- login: `AuthRequired`;
- the session's life, from the daemon: `SessionEnded`, `Note`, `Summarized`.

`changes.changed` (no params) says the overlays changed; whatever shows them rescans.

## 4. Line mode

`sill attach <session>` is the protocol's second consumer (docs/daemon.md decision 11): a
`sill_proto::Client` and a terminal, no store, no jail. A line is a prompt, the agent's text and
tool calls come back, a permission request is answered on the next line (a number, `y`, `n`, or
Enter to cancel), a form is shown and left waiting for a frontend that can fill it, `/cancel`
stops the turn, `/quit` or Ctrl-D detaches and the session keeps running. `--verbose` adds
thoughts, tool call updates and usage. The prompt goes to the agent as typed: the standing
instructions block the shelf prepends (docs/daemon.md §3.2) is still the shelf's.

## 5. Changelog

- **5** — 2026-11-02, frozen. The wire types moved from `sill_core::wire` (and `acp`, `store`,
  `keep`, `ledger`) to `sill_proto::wire`, unchanged on the wire; the envelopes
  (`sessions.command`, `sessions.answer`, `sessions.set`, `agents.set_settings`,
  `session.event`) carry the types instead of opaque JSON — the same bytes. `StopReason` and
  `PermissionOptionKind` are sill's own enums with ACP's strings; an unknown string is kept as
  it came (`Other(String)`), so nothing an agent sends is lost on the wire. `sill daemon schema`, `sill attach`.
- **4** — 2026-11-02: global pins (`pins.list|add|remove`, docs/pins.md).
- **3** — 2026-11-02 (M1c): `sessions.end`, `sessions.checkpoint`, `sessions.new|set|delete|pin|unpin`,
  `memory.note_add|note_remove|compact|forget`, `agents.set_settings`; `SessionEnded`, `Note`,
  `Summarized` events.
- **2** — 2026-11-02 (M1b): `sessions.attach|detach|command|answer`, `session.event`.
- **1** — 2026-11-02 (M0, M1a): `daemon.status|shutdown`, `sessions.list|get|start|stop|log`,
  `memory.*` reads, `settings.get`.
