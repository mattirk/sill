# sill — Sill Frontend Protocol (SFP)

**Direction agreed 2026-11-03:** the public frontend contract is named **Sill Frontend Protocol
(SFP)**. SFP v1 is a design target, not an implemented or frozen public release. Sections 1–5
describe the existing development protocol; [section 6](#6-sfp-v1-design-direction-2026-11-03)
records the work needed before declaring SFP v1 stable. ACP is the separate agent-facing protocol.

Status: protocol **6**, frozen 2026-11-02 (docs/daemon.md M4–M6). From here every change
to a method or a type's shape is a line in the changelog below and a bump of
`sill_proto::PROTOCOL`. A local client asks a mismatched daemon to leave and starts the installed
binary. An SSH client instead compares method schemas and refuses an unsupported operation; it
never replaces or restarts the remote daemon merely to match its own version.

`sill daemon schema` prints the method inventory as one JSON document — this table, each typed method's
params and result as `$ref`s, and the JSON Schema (draft 2020-12) of every type under `$defs` —
for a frontend in another language to generate from. Some payloads remain opaque, notably
`frontend.request`, transcript replay and changes history; this is not yet a complete contract
for an independently generated frontend. The Rust side is `crates/sill-proto`
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


## 6. SFP v1 design direction (2026-11-03)

This section records the agreed direction and proposed implementation requirements. It does not
change development protocol 6 or claim that the work below is implemented.

### Boundary and independence

```text
Frontend <--- SFP ---> Sill daemon / relay <--- ACP ---> Agent
```

SFP serves GUI, terminal and other clients. The daemon owns sessions, persistence, jail policy,
prompt context and host operations. A separate frontend must be implementable from the public
contract without importing `sill-core`, opening `sill.db`, or interpreting backend implementation
files. The existing GTK shelf may stay in the same repository and executable and reuse core
helpers. Splitting crates or binaries is optional packaging work, not a prerequisite for
independent frontends. The daemon remains `sill daemon`.

SFP versions are independent of ACP versions and Sill application versions. The daemon adapts
agent-facing ACP v1/v2 into SFP's conversation model. Do not expose SDK-specific Rust types as
the public contract or require each frontend to implement the agent adapters. The private relay
protocol is separately versioned and remains internal.

The current wire version is 6 and earlier development versions include 1. SFP v1 therefore needs
an unambiguous protocol identity and negotiation scheme; simply resetting `PROTOCOL` to 1 would
collide with the old meaning. Exact handshake fields and legacy transition policy remain to be
specified. A naming change alone does not change the existing `_sill/` method namespace.

### ACP v2 considerations

ACP v2 was announced as a draft on 2026-07-20 and was still documented as draft when reviewed on
2026-11-03. Treat its detailed wire shapes as provisional. Sources:

- [Official draft announcement](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/announcements/acp-v2-draft.mdx): draft status, message identities, broader permission subjects and extensibility.
- [Migration guide](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v2/migration.mdx): lifecycle, replay, patch semantics, capabilities and removal of client execution APIs.

Design SFP for the following semantics now, while retaining ACP v1 support:

- **Acceptance and activity are separate.** ACP v2 acknowledges insertion of a user message;
  completion arrives through state updates. SFP should distinguish submission status, foreground
  activity and background work. A foreground idle state does not establish that no process is
  running or that automatic cleanup is safe. Do not freeze the runner's single `busy` flag as
  the public model. Define cancellation scope and distinguish a cancellation request from its
  confirmed outcome.
- **Transcript items have durable identities.** Messages, tool calls and their updates need IDs,
  with documented omitted/null/replacement/append behavior. Replaying or reconnecting must not
  duplicate items. Specify ordering, cursor scope, snapshot-to-live handoff, retention gaps and
  when a client must replace its snapshot. IDs and cursors exposed by SFP must remain meaningful
  across main-daemon restarts; define what happens when the underlying agent is replaced.
- **Submission recovery is explicit.** Proposed SFP contract: client-generated submission IDs,
  durable daemon acceptance and a status query. Distinguish accepted by Sill, inserted into the
  agent conversation, rejected, and outcome unknown. ACP v2 does not make retry after a lost
  prompt reply automatically safe. Sill must not infer insertion from identical message text or
  replay omission, or promise exactly-once execution after loss of agent/relay state. Specify
  submission-ID scope and deduplication retention before freezing the API.
- **Questions are independent objects.** Permissions and forms have their own IDs, lifecycle,
  title and optional description/subject; a tool-call ID need not exist. Preserve the first-answer
  rule across frontends and broadcast resolution. Document stale answers and cancellation.
- **Compatibility is deliberate.** Negotiate protocol versions and optional capabilities;
  document handling of unknown fields, enum values and content types. Preserve unfamiliar display
  data where possible, but never turn an unknown authorization meaning into permission. Additive
  features should not force coordinated frontend/daemon releases.
- **Display and execution remain distinct.** ACP v2 removes client filesystem and terminal
  execution methods and adds agent-owned terminal display updates. SFP still needs host file
  operations, interactive PTYs, managed processes, ports and Keep/Revert. Agent terminal output
  does not grant an interactive shell. Structured agent-reported changes are presentation data;
  Sill's own filesystem checks remain authoritative for host mutations.

### Work required before the v1 freeze

1. Specify every operation, result, event and error in language-independent schemas, including
   today's opaque `frontend.request`, stored transcript and changes-history payloads. Document
   path ownership (box versus frontend), binary data, limits and pagination. Choose whether to
   retain a typed dispatcher or expose separate methods as an implementation design step.
2. Specify the conversation and recovery semantics above, including concurrent frontends,
   question resolution and retry rules for mutating operations beyond prompts.
3. Specify byte-stream framing, token ownership/expiry, backpressure, end-of-stream, cancellation,
   reconnection and PTY resize/exit semantics. Socket and SSH clients must observe the same contract.
4. Specify negotiation, extension rules, lifecycle ownership and migration from development
   protocol 6. A running daemon's compatibility must be evaluated independently of its build ID.
5. Exercise the contract with a small independent client importing no `sill-core` types. Include
   reconnect/replay, duplicate submission, multiple observers answering one question, unknown
   variants and stream teardown in conformance checks. A Rust schema-generation test alone is
   insufficient evidence of frontend independence.

### Open product decisions

Asked through a form on 2026-11-03; unanswered choices remain open, not implicitly approved.
Technical details above still need a concrete specification after these choices.

| Decision | Recommended choice | Alternative |
|---|---|---|
| Transition from protocol 6 | Coordinated upgrade to SFP v1, without a legacy compatibility layer | Serve both during a transition period |
| Incompatible daemon | Explain mismatch and offer explicit restart/upgrade; no automatic replacement | Retain automatic replacement locally, explicit action remotely |
| ACP v2 implementation timing | Design for v2 now; gate its adapter until stable and keep v1 working | Require an experimental v2 adapter before freezing SFP v1 |
| Prompt submitted during work | Queue by default; offer steering when supported | Require explicit queue/steer choice, or steer by default with queue fallback |
