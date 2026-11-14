# sill — the daemon protocol

Status: protocol **6**, frozen 2026-11-02 (docs/daemon.md M4–M6). From here every change
to a method or a type's shape is a line in the changelog below and a bump of
`sill_proto::PROTOCOL`. A local client asks a mismatched daemon to leave and starts the installed
binary. An SSH client instead compares method schemas and refuses an unsupported operation; it
never replaces or restarts the remote daemon merely to match its own version.

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
- **SSH**: the system SSH client runs `sill daemon --stdio` on a saved box. The stdio command
  bridges to that host’s existing daemon; it does not create a second store writer. Select a saved
  box with `SILL_BOX=NAME` or `sill box connect NAME`. SSH retains the person’s authentication and
  host-key policy.
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
  Before the live snapshot, `_sill/session.replay {key, entries, total}` supplies the stored
  transcript atomically with subscription. A frontend replaces its stored-history view from this
  replay before applying subsequent events; it must not stitch an earlier separate read onto the
  live stream. The core-owned stored `Entry` shape is currently opaque in the standalone schema.
- **Questions**: a permission request or a form arrives as an event with a `request` number and
  waits — with nobody attached too — until some frontend answers it with `sessions.answer`; the
  first answer counts and every attached frontend then gets `Resolved {request}`.
- **Timeouts**: a single Rust client deadline covers outbound queuing, sending and the answer
  (120 s by default, 5 s for `daemon.status` and `daemon.shutdown`). Timeout closes the transport
  so a blocked send cannot strand subsequent calls. Long browser login and registry install progress use streams.
- **Byte streams**: `frontend.request` operations `OpenPty`, `OpenPort`, `HostLogin` and `Install` return a
  one-use token. Open the same transport again, write `{"_sill_stream":"TOKEN"}\n`, then exchange
  raw bytes. Tokens expire after 60 s if unused. `ResizePty`/`PtyCwd` use the original control
  connection and token; descriptors never cross the frontend protocol.

## 2. Methods

| Method | Kind | Params | Result | What |
|---|---|---|---|---|
| `_sill/frontend.request` | request | any | any | Typed host-data operations for the shelf (sill_core::frontend::Request); suboperation shapes are not yet in this schema. |
| `_sill/daemon.schema` | request | `Empty` | any | Protocol schema and supported method shapes, for remote capability negotiation. |
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
| `_sill/session.replay` | notification | any | — | An attach’s atomic stored transcript snapshot: key, entries and total (core-owned Entry shape). |
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
yet (`changes.history`, `settings.get`, `session.replay` and `frontend.request` carry core-owned types). A generated
client still needs those definitions; schema negotiation cannot validate the opaque suboperation
shapes of `frontend.request`. Its request is a serde externally tagged enum (for example
`{"Metadata":{"path":"/home/me/file"}}`); its result is the operation’s value, not another enum.

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
thoughts, tool call updates and usage. Both line mode and the shelf send the plain prompt. The
daemon prepends current jail facts, standing instructions, scoped recall and session pin context,
and restores remembered agent controls independently of either frontend.

## 5. Changelog

- **6** — 2026-11-02. `daemon.schema` for remote method negotiation; `frontend.request` for
  daemon-host data and actions (store reads, files, config/instructions, registry, uploads, denied
  hosts, processes and logs), terminal/port/login one-use byte streams and their control operations.
  The latter’s suboperation types are in `sill_core::frontend` and are opaque in this schema.
  `session.replay` supplies stored transcript rows atomically with attachment, before the live
  snapshot, preventing a missed or duplicated tail between a separate history read and attach.
  SSH/stdio is an alternative transport. Main-daemon recovery uses a private ACP relay and
  transactional event cursors; that private worker protocol is not a frontend API.
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
