# sill — Sill Frontend Protocol (SFP)

**Implementation in progress, 2026-11-03:** the current control connection uses
`_sill/initialize` with identity **`sfp/1-dev.1`**. This development identity implements the
conversation slice described below; it does not claim the complete `sfp/1-draft.1` baseline or
stable `sfp/1`. Clients and daemon upgrade together. A mismatch requires explicit replacement;
a failed handshake never authorizes restarting a daemon. Private relay continuity is retained
where its protocol remains compatible.

The implemented public conversation surface is `_sill/sessions.attach`,
`_sill/sessions.snapshot`, `_sill/sessions.detach`, `_sill/sessions.submit`,
`_sill/submissions.get`, `_sill/submissions.cancel`, `_sill/submissions.resume`,
`_sill/questions.answer`, `_sill/sessions.control`, `_sill/sessions.cancel`,
`_sill/uploads.create`, `_sill/uploads.get` and `_sill/session.event`, plus `_sill/daemon.schema`.
The schema command exports the concrete records and method shapes. Counters are decimal strings;
records/events use named objects and `type` tags. `scripts/sfp_client.py` is an independent Python
consumer and schema validator (install `scripts/requirements-sfp.txt`).

Conversation changes, public sequence, source deduplication and runner state commit before SFP
publication and relay ACK. Snapshots have connection-bound 60-second leases, at most 128 records
and 4 MiB per page, 32 MiB per snapshot, eight leases per connection and a 128 MiB aggregate
snapshot limit. Replay above 4096 events selects a snapshot. Current development storage retains
all public events; retention/compaction is unfinished. Agent text/thought output splits into items of at most 256 KiB. A large tool or other record
exceeding the page limit returns `limit_exceeded`; segmentation of those records remains freeze work. Control messages are
limited to 8 MiB including newline, prompt text to 1 MiB JSON-encoded and initialization must arrive within ten seconds.

ACP v1 queues submissions during activity; it does not advertise steering. Prepared payload and
relay command ID are persisted before sending. A successful ACP prompt result establishes
insertion/completion (or the agent's reported cancellation). Prompt RPC errors report uncertainty.
Replacement holds unsent submissions and marks possibly dispatched work unknown. Existing
transcript rows import once with new stable identities in a distinct historical generation;
matching text never establishes continuity. Permission/form answers persist the winning answer
and delivery intent before sending. New relays advertise outcome evidence; their resolution confirms handoff to the ACP question
handler, without claiming that the remote agent acted on it. Adopted older relays retain their
conversations, but ambiguous cancellation or answer-delivery evidence is reported as unknown.

Uploads are immutable session-scoped objects with client IDs and bounded base64 data. Inline
attachments currently support images; file attachments reference uploaded objects. Upload IDs
retain content identity independently of their staged jail file. The daemon stores an inline
attachment as an upload when it accepts the submission (`inline-<index>-<submission_id>`), so
submission records reference attachment bytes and never carry them; a frontend fetches what it
displays with `_sill/uploads.get`. Records journaled before this change may still hold inline data.
An uploaded PNG, JPEG, GIF or WebP reaches the agent as an image block when the agent advertised
image support; any other upload, and every upload to an agent without it, is a staged file.

Known follow-ups in this area: inline upload IDs (`inline-<index>-<submission_id>`, cut to 128
bytes) share the client's upload ID namespace, so a client upload of that name or two long
submission IDs with a common 119-byte prefix fail with `id_conflict`; the shelf fetches every
displayed upload again whenever it rebuilds a view, although uploads are immutable and could be
cached by session and upload ID; and the adapter's once-per-update-kind log set
(`AdapterState.unhandled`) is persisted with the adapter state, so it logs once per session rather
than once per daemon run (`#[serde(skip)]` would fix it). The plan's step marks (✓ ◐ ○) are
written in both `sfp_client::plan_text` and `Conversation::set_plan`.

ACP v1 updates beyond text and tools are projected too: `plan` becomes a `plan` item per turn
(one `plan_entry` block per step with `status` and `priority`; later plans patch its content),
`notice` a `notice` item (`severity` on its block), `available_commands_update` the session's
`commands`, and a `session_info_update` title the session's `agent_title` (sill's own title is
unaffected). Other update kinds are logged by the daemon and never become conversation items.

Public session administration is now implemented too (2026-11-03):

| Method | Request | Result |
|---|---|---|
| `_sill/sessions.list` | `{after?, limit?}` | `{sessions, next}` |
| `_sill/sessions.get` | `{session_id}` | public session metadata |
| `_sill/sessions.create` | `{create_id, agent, policy, pins?, desktop?, memory_scope?, follows?, remember_pins?, ghost?}` | `{session_id}` |
| `_sill/sessions.update` | `{session_id, change}` | `{}` |
| `_sill/sessions.start` | `{session_id}` | `{session_id, fresh}` |
| `_sill/sessions.stop` | `{session_id}` | `{}` |
| `_sill/sessions.end` | `{session_id, reason}` | `{pending, deleted}` |
| `_sill/sessions.checkpoint` | `{session_id}` | `{}` |

These methods require no `sill.internal` capability. Metadata is an explicit public record,
including settings, pins and decimal-string Unix-second timestamps; it exposes no ACP session
identity or shim socket. Every operation uses an exact session ID. The CLI resolves unique
prefixes/titles locally and rejects ambiguity. Shelf session listing and lifecycle writes, the
CLI and the Python client's `list/create/get/update/start/stop/end/checkpoint` use this surface.
Remaining internal administration handlers are temporary same-build APIs, not a compatibility
contract. Public `update` uses a named, `type`-tagged change; unknown change kinds are rejected.
The metadata write and corresponding conversation session event commit atomically.

Lists default to 64 records, allow 1–128, and cap each response at 4 MiB. The opaque `next`
cursor becomes the next request's `after`. Keyset ordering is descending immutable creation
time and ID, so renaming/touching sessions cannot reorder them between pages. Each page reads
current metadata, not an immutable list snapshot: concurrent insertions may appear after the
cursor or be missed before it; deletions disappear. Start a new traversal to refresh membership.

Create IDs are daemon-store scoped, 1–128 bytes without control characters. Identical normalized
input returns the original identity across disconnect/restart; changed input returns
`id_conflict`. Creation, configuration and the retry receipt are one transaction. Display-path
text is excluded from retry identity. Deleting a session clears its identity from the receipt;
a subsequent retry returns `gone`, never creates another session. Receipts retain only SHA-256
fingerprints of the key and input and the live session ID. They currently have no time expiry;
the ledger is bounded at 65,536 entries (further new IDs return `limit_exceeded`). Receipt
retention/compaction remains freeze work. Creation does not start a jail or agent.

Other lifecycle mutations **must not be blindly retried** after timeout/connection loss: read
metadata and conversation state first; uncertainty does not establish that nothing happened.
The shelf drops a closed connection but does not resubmit these actions. `start` starts/reuses
the jail, `attach` starts the agent, and `stop` preserves session history. Starting/checkpointing
an ended session requires an explicit `update` with `reopen`. `end.reason` is `close`, `idle`
or `quit`; `pending` means the summary is still running, and `deleted` means deletion was
selected (jail teardown and row removal can finish asynchronously). Checkpoint completion is
observed through the journaled summary/session update. Update input is limited to 1 MiB.

Pins use `{bytes_base64, display}` host paths: exact absolute Linux bytes are authoritative,
not the frontend's filesystem or the display text. At most 128 pins and 4096 decoded bytes per
path are accepted. The existing pin storage/jail pipeline still requires UTF-8: non-UTF-8 input
currently returns `unsupported`, never a lossy replacement. Completing that pipeline remains
part of the lossless-path milestone.

Independent lifecycle conformance (with `jsonschema` installed in a venv):

```sh
SILL_SFP_PYTHON=/path/to/venv/bin/python cargo test -p sill-shelf --no-default-features --test recovery independent_python_client -- --ignored
```

The fixture supplies a scripted ACP agent; Python creates the session, submits work, reconnects
with its saved view after the main daemon is killed, answers the recovered question and ends
the session. Every Python request/result/event is checked against the exported schema, without
negotiating internal APIs or reading the database. Rust tests also cover create rollback,
conflicting/repeated IDs, deletion tombstones, paging and lifecycle metadata across restart.

Host/session administration and presentation operations still awaiting public SFP schemas are
under `_sill/internal/…`, marked `x-sill-internal` in the exported schema. These are temporary
same-build implementation APIs, not protocol 6 compatibility handlers. The old `_sill/…` names
are removed when their public replacement is not yet implemented. Public host methods, lossless
path contracts, persistent terminal replay, full mutation recovery, retention bounds, and the
experimental ACP v2 adapter remain implementation work before `sfp/1` can be declared complete.

**Direction agreed 2026-11-03:** the public frontend contract is named **Sill Frontend Protocol
(SFP)**. SFP v1 is a design target, not an implemented or frozen public release. Sections 1–5
record the historical development protocol and the remaining internal method inventory; [section 6](#6-sfp-v1-design-direction-2026-11-03)
records the work needed before declaring SFP v1 stable. [Section 7](#7-sfp1-draft1-proposed-contract)
is the first concrete draft, not an implemented API. ACP is the separate agent-facing protocol.

Historical baseline: protocol **6**, frozen 2026-11-02 (docs/daemon.md M4–M6), preceded the SFP
cutover. Its automatic local replacement and remote structural-schema negotiation are removed.
The current initialization identity governs control compatibility; the numeric status field is
an internal historical diagnostic. The table below names the remaining internal methods with
their new namespace. They require the explicitly negotiated `sill.internal: 1` capability and
are intended only for same-build clients while their public replacements are implemented.

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
- **Method names** are ACP extension methods, `_sill/internal/<group>.<name>`, so a connection could carry
  ACP's own `session/*` traffic one day.
- **One connection, one subscriber set**: `sessions.attach` subscribes the connection to a
  session's `session.event` notifications (the session's current state first — capabilities,
  controls, a waiting login, the turn in flight with its text so far, unanswered questions);
  `sessions.detach`, a `Quit` command or the connection closing unsubscribes. The daemon's queue
  per connection is bounded; a frontend that stops reading is cut off and attaches again.
  Before the live snapshot, `_sill/internal/session.replay {key, entries, total}` supplies the stored
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
| `_sill/internal/frontend.request` | request | any | any | Typed host-data operations for the shelf (sill_core::frontend::Request); suboperation shapes are not yet in this schema. |
| `_sill/internal/daemon.schema` | request | `Empty` | any | Protocol schema and supported method shapes, for remote capability negotiation. |
| `_sill/internal/daemon.status` | request | `Empty` | `DaemonStatus` | Version, protocol, pid, mode and session counts of the daemon. |
| `_sill/internal/daemon.shutdown` | request | `Empty` | `{}` | Ask the daemon to leave; sessions keep running and the next daemon adopts them. |
| `_sill/internal/sessions.list` | request | `Empty` | `[Session]` | Every session in the store, newest first. |
| `_sill/internal/sessions.get` | request | `Key` | `Session` | One session by id, id prefix or exact title. |
| `_sill/internal/sessions.new` | request | `NewSession` | `Key` | Create a session row; answers its id. |
| `_sill/internal/sessions.set` | request | `SessionSet` | `{}` | Change one field of a session's row. |
| `_sill/internal/sessions.delete` | request | `Key` | `{}` | Stop the jail, remove the session's state and its row. |
| `_sill/internal/sessions.pin` | request | `Pin` | `{}` | Pin a path in a session (bound at its next start). |
| `_sill/internal/sessions.unpin` | request | `Pin` | `{}` | Unpin a path. |
| `_sill/internal/sessions.start` | request | `Key` | `Started` | Start a session's jail, or find it running. |
| `_sill/internal/sessions.stop` | request | `Key` | `{}` | Stop a session's jail; the row stays. |
| `_sill/internal/sessions.log` | request | `LogRequest` | `Text` | The tail of a session's log. |
| `_sill/internal/sessions.attach` | request | `Attach` | `{}` | Start jail and agent when needed and subscribe this connection to the session's events; its current state comes first. |
| `_sill/internal/sessions.detach` | request | `Key` | `{}` | Unsubscribe; the agent stays the daemon's. |
| `_sill/internal/sessions.command` | request | `SessionCommand` | `{}` | A prompt, cancel, mode, option, login or quit for a running session; a prompt during a turn is refused. |
| `_sill/internal/sessions.answer` | request | `SessionAnswer` | `{}` | The answer to an agent's question (permission or form); the first answer counts. |
| `_sill/internal/session.replay` | notification | any | — | An attach’s atomic stored transcript snapshot: key, entries and total (core-owned Entry shape). |
| `_sill/internal/session.event` | notification | `SessionEvent` | — | One event of a session, to every connection attached to it. |
| `_sill/internal/sessions.end` | request | `End` | `Ending` | End a session as closing its tab would: the summary turn when the agent can be asked, the row written or deleted, the jail stopped. |
| `_sill/internal/sessions.checkpoint` | request | `Key` | `{}` | Summarize now, without ending; the summary comes as a Summarized event. |
| `_sill/internal/sessions.changes` | request | `Key` | `Count` | How many unkept writes a session has outside its pins. |
| `_sill/internal/sessions.jail_view` | request | `Key` | `JailView` | A session's jail layout, for showing files as the agent sees them. |
| `_sill/internal/pins.list` | request | `Empty` | `[path]` | The global pins (docs/pins.md). |
| `_sill/internal/pins.add` | request | `GlobalPin` | `{}` | Add a global pin (canonicalized; refused inside another or in sill's state). |
| `_sill/internal/pins.remove` | request | `GlobalPin` | `{}` | Remove a global pin. |
| `_sill/internal/changes.list` | request | `Empty` | `ChangesList` | Every session's overlay changes and undoable keeps, and the pinned repositories' git status. |
| `_sill/internal/changes.diff` | request | `DiffOf` | `Text` | One change's diff. |
| `_sill/internal/changes.keep` | request | `ChangesKeep` | `Done` | Copy changed paths to the host; each checked against the hash it was listed with. |
| `_sill/internal/changes.revert` | request | `ChangesPaths` | `Done` | Put the host's version back in the session. |
| `_sill/internal/changes.undo` | request | `ChangesUndo` | `Done` | Undo keeps. |
| `_sill/internal/changes.git_revert` | request | `GitRevert` | `Text` | A pinned repository's path back to HEAD. |
| `_sill/internal/changes.history` | request | any | any | The ledger across time (SPEC §3.6); params and result are sill_core::history's types, not in this schema yet. |
| `_sill/internal/changes.changed` | notification | `Empty` | — | The overlays changed (a sweep, a keep, a turn, a deletion): whatever shows them rescans. |
| `_sill/internal/memory.list` | request | `Empty` | `[MemorySession]` | Ended sessions, newest first. |
| `_sill/internal/memory.show` | request | `Key` | `Text` | An ended session's summary and notes, as the CLI prints them. |
| `_sill/internal/memory.search` | request | `MemorySearch` | `Text` | Full-text search over summaries and transcripts. |
| `_sill/internal/memory.transcript` | request | `Transcript` | `Text` | A session's transcript. |
| `_sill/internal/memory.note_add` | request | `Note` | `{}` | Add a note to a session's memory. |
| `_sill/internal/memory.note_remove` | request | `Note` | `{}` | Remove a note by its time. |
| `_sill/internal/memory.compact` | request | `Empty` | `Compacted` | Compaction and retention now. |
| `_sill/internal/memory.forget` | request | `Empty` | `Count` | Delete every ended session. |
| `_sill/internal/agents.set_settings` | request | `AgentSettingsSet` | `{}` | An agent type's default settings. |
| `_sill/internal/settings.get` | request | `Empty` | any | sill's settings (sill.toml); the type is sill_core::config's, not in this schema yet. |

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
collide with the old meaning. Section 7 proposes the identity and handshake. The transition is a
coordinated upgrade of clients and daemon, with no protocol 6 compatibility layer. A naming change
alone does not change the existing `_sill/` method namespace.

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
- **Prompts during work steer when supported.** By default, deliver the prompt into active work
  when the agent supports steering; otherwise queue it. Specify how that support is determined
  and how queued versus steering submissions are reported, independently of acceptance and
  completion.
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
   path ownership (box versus frontend), binary data, limits and pagination. Section 7 proposes
   separate public methods in place of the opaque dispatcher.
2. Specify the conversation and recovery semantics above, including concurrent frontends,
   question resolution and retry rules for mutating operations beyond prompts.
3. Specify byte-stream framing, token ownership/expiry, backpressure, end-of-stream, cancellation,
   reconnection and PTY resize/exit semantics. Socket and SSH clients must observe the same contract.
4. Specify negotiation, extension rules, lifecycle ownership and migration from development
   protocol 6. A running daemon's compatibility must be evaluated independently of its build ID.
   On incompatibility, explain the mismatch and require explicit action before replacing or
   restarting either a local or remote daemon. The initial SFP v1 transition upgrades clients
   and daemon together; it does not retain a protocol 6 compatibility layer.
5. Exercise the contract with a small independent client importing no `sill-core` types. Include
   reconnect/replay, duplicate submission, multiple observers answering one question, unknown
   variants and stream teardown in conformance checks. A Rust schema-generation test alone is
   insufficient evidence of frontend independence.
6. Implement and exercise an experimental ACP v2 adapter before freezing SFP v1, validating the
   conversation lifecycle and replay against actual v2 behavior. Keep draft v2 support opt-in
   and retain ACP v1 support; the v2 adapter is a freeze prerequisite, not deferred until after
   SFP v1 stabilizes.
7. Bound transport input and resource use: define and enforce maximum JSON message sizes,
   idle-handshake deadlines, connection limits and pending-stream limits. Bound stream-expiry
   bookkeeping as well; a thread per pending token must not allow unbounded resource growth.
   Verify rejection and cleanup when limits or deadlines are reached, alongside the existing
   bounded outbound queues.
8. Specify the transport trust contract. Unix-socket peer credentials and SSH authenticate the
   user account; authenticated frontends currently receive the daemon's full authority, including
   host operations. Feature negotiation is not authorization. Keep the daemon socket and its
   descriptors inaccessible to agent jails, whose processes may share the person's uid, and
   regression-check that isolation. Any future TCP, browser or restricted-client transport must
   define its authentication and authorization boundary before exposure; do not assume that
   changing to HTTP or adding encryption supplies that boundary.

### Product decisions (answered 2026-11-03)

The person answered these interactively. They govern the SFP v1 work; they do not describe
changes already implemented in development protocol 6. Section 7 drafts the technical details;
its open choices do not reopen these decisions.

| Decision | Chosen behavior |
|---|---|
| Transition from protocol 6 | Upgrade clients and daemon together; no legacy compatibility layer. |
| Incompatible daemon | Explain the mismatch and require explicit action before replacement or restart, locally and remotely. |
| ACP v2 implementation timing | Validate SFP with an experimental ACP v2 adapter before freezing v1; keep draft support opt-in and retain ACP v1. |
| Prompt submitted during work | Steer active work when supported; otherwise queue the prompt. |
| Linux filenames | Support lossless path bytes plus display text, including filenames that are not valid UTF-8. |
| Queued prompt controls | Cancel individual queued prompts and explicitly resume held prompts; preserve queue order. No editing or reordering in v1. |
| Interactive terminal disconnect | Keep the terminal alive within its session or explicit lifetime and allow reattachment to the same terminal. |
| Terminal output recovery | Replay bounded recent output on reattachment and explicitly report gaps; do not retain an unlimited terminal byte log. |
| Independent validation client | Build the first independent client in Python using exported schemas. |

## 7. sfp/1-draft.1 proposed contract

**Status: first design draft, 2026-11-03; not implemented.** The requirements in this section
describe the proposed target, not development protocol 6. They incorporate the second-opinion
review into the contract. Section 6's product decisions remain settled. The open choices at
the end must be resolved before freezing; method names and shapes below may change during drafts.
This is a semantics draft, not yet the complete machine-readable schema or method inventory.

### 7.1 Ownership and migration

SFP is Sill's own JSON-RPC protocol. It retains the `_sill/` method namespace but does not carry
ACP `session/*` methods or require an ACP library in a frontend. The daemon adapts agent behavior
into SFP and owns transcript construction, prompt context, questions, policy and persistence.
Agent-reported file changes and terminal output are display data, not authority to execute host
operations. The same contract applies over the local socket and SSH/stdio.

Before changing SFP wire shapes, separate the relay's private command/event types from public
frontend types. Convert at the main daemon boundary. Private types may normalize ACP, but must
not depend on SFP serialization or expose SDK types publicly. A change confined to SFP must not
require a relay protocol bump. This does not promise that genuinely incompatible relay changes
can preserve a running agent; the initial split's deployment needs its own migration check.

Upgrade protocol 6 clients and daemon together; do not implement a legacy compatibility layer.
Detecting incompatibility never authorizes replacing or restarting a local or remote daemon.
Explain the mismatch and wait for an explicit user action. Build IDs are diagnostic, not protocol
compatibility tests. A failed handshake must not start a second daemon beside an existing one.

### 7.2 Initialization and compatibility

Control messages are UTF-8 JSON-RPC 2.0 objects, one per line. Request IDs correlate replies;
they are not submission IDs or replay cursors. Replies may arrive out of request order. Clients
must dispatch by ID, and must not infer execution order from the order of replies. No JSON-RPC
batches in this draft. Session state changes are ordered by session event sequence instead.

The first control request is `_sill/initialize`. Example request and result:

```json
{"jsonrpc":"2.0","id":1,"method":"_sill/initialize","params":{"protocols":["sfp/1-draft.1"],"client":{"name":"example-client","version":"0.1"},"capabilities":{}}}
{"jsonrpc":"2.0","id":1,"result":{"protocol":"sfp/1-draft.1","daemon":{"version":"example"},"capabilities":{},"limits":{"max_message_bytes":8388608,"max_inline_bytes":1048576,"snapshot_ttl_seconds":60}}}
```

The daemon selects the first offered protocol it supports. No overlap returns `incompatible`
with its supported identities and leaves the connection uninitialized; only another initialize
attempt or closing the connection is valid. Other methods before success return
`not_initialized`. Reinitializing a successful connection returns `invalid_state`.
Identities are exact strings: `sfp/1-draft.N` during development and `sfp/1` at freeze.

Capabilities are named, versioned optional features. Unknown offers are ignored; the result
contains only mutually supported optional features. Empty capabilities provide the complete
baseline conversation contract. Required new state semantics need a negotiated feature or a
new protocol identity. Agent abilities, such as steering, are separately reported in session
state and may change when the agent changes; handshake capabilities describe SFP support.

The example limits are illustrative, not chosen defaults. Limits apply to encoded message bytes
(including the newline) and decoded inline bytes respectively. A fixed pre-initialize message
ceiling and the mandatory limit inventory remain freeze work. `_sill/daemon.schema` must describe
the selected protocol's methods, notifications, errors, limits and capability requirements; it
must not expose protocol 6 shapes under an SFP identity.

### 7.3 Identities, state and event ordering

All public IDs are opaque strings. Mutations use full `session_id` values, never ambiguous titles
or prefixes. Session IDs are never reused. Item, turn, question and agent-generation IDs are
daemon-assigned and stable for their lifetime; client submission and answer IDs are described
below. A frontend must not interpret ACP IDs or transient request numbers as public identities.

A cursor is `{session_id, seq}`. `seq` is a canonical unsigned decimal string, starting at `"0"`
for the initial state, with no leading zeros. Strings avoid loss in JavaScript's number type.
Each committed session event advances it by one. There is no ordering across sessions. A daemon
restart or agent replacement does not reset it. A deleted session cannot be resumed as a new
session with the same identity. Restoring an older database without cursor continuity must
invalidate old session identities rather than reuse sequences for different events.

The public sequence is separate from relay sequence numbers and transcript row numbers. A relay
sequence is worker-local; a transcript row can mutate. Persist each state change, its public
event and its source-event deduplication mapping atomically before publication or relay ACK.
Source identities must include their relay/agent generation so reused source numbers cannot
alias earlier events. Re-delivery of an already committed source event does not mint a new
public event or append text twice. Snapshot state must be recoverable at the published cursor.
These are observable durability guarantees, not a requirement to use a particular database layout.

Session state comprises:

| Object | Required meaning |
|---|---|
| Session | Lifecycle, current `agent_generation_id`, agent capabilities and activity. |
| Item | `item_id`, `created_seq`, nullable `turn_id`, nullable `submission_id`, originating generation, kind and content. |
| Turn | `turn_id`, generation and activity/outcome when a meaningful turn grouping exists. |
| Submission | Client input identity, daemon acceptance, delivery, insertion and outcome. |
| Question | Independent `question_id`, generation, kind, title, optional description/subject/tool reference, state and answer delivery. |

Items cover user/agent messages, thoughts, tool calls, terminal display and daemon notes. Item
order is `created_seq`; updates do not move an item. `turn_id` may be null for unsolicited or
background work. A steering submission need not start a new turn. Content blocks have stable
`block_id`s within their item and tagged content types. Exact content variants, tool fields and
form schemas are open schema work; the lifecycle and patch rules below apply to all of them.

The single notification envelope is `_sill/session.event`:

```json
{"jsonrpc":"2.0","method":"_sill/session.event","params":{"session_id":"s1","subscription_id":"sub1","seq":"43","event":{"type":"item.text_appended","item_id":"i1","block_id":"b1","text":" world"}}}
```

Baseline event families and application rules:

| Event | Effect |
|---|---|
| `item.created {item}` | Insert a new complete item with a previously unused ID. |
| `item.patched {item_id, set}` | Replace the named mutable fields; omitted fields stay unchanged. |
| `item.text_appended {item_id, block_id, text}` | Append exactly this string to an existing text/thought block. |
| `item.removed {item_id, reason}` | Remove an item from the current view; never reuse its ID. |
| `session.updated {session}` | Replace the complete current session state, including activity/capabilities. |
| `turn.updated {turn}` | Insert or replace the complete turn record. |
| `submission.updated {submission}` | Insert or replace the complete submission record. |
| `question.updated {question}` | Insert or replace the complete question record, including terminal resolutions. |

`set` uses an allowlisted schema for the item kind, not arbitrary JSON paths. IDs, kind and
creation sequence are immutable. Null clears only a nullable field; null is otherwise invalid.
Arrays/objects in `set` replace the entire field, not a recursive merge. Replacing `content`
replaces the block list; retained blocks keep their IDs and removed block IDs are not reused.
Appending and replacing text are distinct operations. Each event applies atomically. A missing
patch target or invalid transition is a synchronization failure: stop applying and obtain a
fresh snapshot instead of fabricating an item. No public event may expose only half a state change
that violates these rules; related updates needing atomicity must share one event shape before freeze.

Clients ignore duplicate events at or below their last applied sequence, detect a gap above the
next expected sequence, and reattach to recover. Every subscriber receives the same sequenced
state events; capability-specific presentation must not introduce sequence gaps. Unknown fields
are ignored and unknown display content is preserved as an opaque block/placeholder. Unknown
enum strings are retained but never treated as a known successful or authorized state. Unknown
state-changing event types are a compatibility error, not silently skipped. An optional extension
may be ignored only when its negotiated contract explicitly declares it nonessential to state
reconstruction. Unknown authorization meanings never grant permission.

### 7.4 Attach, snapshots and retention

`_sill/sessions.attach {session_id, since?}` subscribes this connection. `since` is the last cursor
the client fully applied **and whose state it still holds**, not the last event it merely received.
Omitting it asks for a snapshot. The response precedes all notifications for the new subscription
and chooses one of these shapes (shown as notation, not literal JSON):

```text
{subscription_id, mode: "replay", after: Cursor, through: Cursor}
{subscription_id, mode: "snapshot", snapshot: {token, at: Cursor, expires_at}}
```

For replay, establish a boundary N atomically with subscription, replay every event after `since`
through N, then continue with N+1 onward. `through` identifies N. A cursor equal to N is valid
and has no replay tail. A foreign or future cursor returns `invalid_cursor`; a valid cursor older
than retained events selects snapshot mode. The daemon may also choose snapshot mode to bound
replay cost. The client must implement both modes.

For snapshot mode, capture immutable state at N and subscribe to events after N at the same
boundary. `_sill/sessions.snapshot {token, page?}` returns `{at, records, next_page}`; null
`next_page` ends the snapshot. Records are tagged session/item/turn/submission/question objects.
Every page belongs to the same snapshot and can be retried using the same page token. Tokens are
opaque, connection/subscription-bound and expire at the advertised time; expired reads return
`snapshot_expired`. Page size and token/resource limits are part of the schema work.

The client buffers live events after N while reading pages, then atomically replaces its session
view, sets its cursor to N and applies the buffered tail in order. Never mix old local history
with a replacement snapshot. The snapshot includes all retained items, current session/turn state,
open questions, answered questions with pending/unknown delivery and pending/uncertain submissions.
Historical terminal record pagination must not omit state needed for recovery. Every item that
may still receive a patch must be present, along with records needed to interpret its references;
retention cannot discard an item and later emit a patch against it. The precise history-retention
policy remains open.

Both ends bound queues. If the daemon cannot preserve ordered delivery, it closes the connection;
if a client cannot buffer the snapshot tail, it abandons that synchronization and reattaches.
Neither silently drops state events. A slow client may repeatedly need a new snapshot; flow-control
tuning must be validated before freeze. There is no promise to retain every token delta forever.
Compaction may discard old events only while leaving a consistent snapshot and the separate
deduplication records required below.

There is one active subscription per session per connection. Reattach replaces it and invalidates
its snapshot tokens. `subscription_id` distinguishes old queued notifications; ignore those once
the replacement attach response arrives. `_sill/sessions.detach {session_id, subscription_id}`
invalidates that subscription only. Detach or connection loss does not cancel work or questions.
The draft retires protocol 6's separate opaque `session.replay` transcript format.

### 7.5 Submission acceptance, steering and recovery

`_sill/sessions.submit {session_id, submission_id, text, attachments}` returns the durable
submission record. `_sill/submissions.get {session_id, submission_id}` retrieves its current state.
The daemon constructs transcript items and agent context; no client-written `entry` is accepted.
Attachments reference immutable uploaded objects (or bounded inline content), never an implicit
path on the frontend machine. Their exact schema remains open.

Deduplicate by `(session_id, submission_id)`. A retry with the same semantic client input returns
the existing record; reuse with different text, attachment content or other input returns
`id_conflict`. Transport IDs and daemon-added context do not participate in input identity.
Attachment identity must not depend on an expiring download URL. Retain input fingerprints and
deduplication records or tombstones for the session lifetime, independently of event retention.
Such tombstones retain enough input identity and outcome to serve the promised retry and status
results; retaining just the ID is insufficient. The same rule applies to answer deduplication.
Session deletion makes subsequent mutations fail; it never recreates a session or a submission.

Validation failures before recording acceptance return an RPC error and perform no delivery.
Once accepted, commit the submission and its update event before returning success. An accepted
record has independent fields:

| Field | Values and interpretation |
|---|---|
| `acceptance` | `accepted`: Sill has durable responsibility for the submission, not proof of agent receipt. |
| `delivery` | `queued`, `direct`, `steering`: current placement or delivery mode; queued records also expose `held` and a nullable `hold_reason`. |
| `insertion` | `pending`, `confirmed`, `rejected`, `unknown`: knowledge of insertion into the agent conversation. |
| `outcome` | `pending`, `completed`, `cancelled`, `failed`, `unknown`: known processing outcome, separately from insertion. |

Include structured reasons for rejection, failure or uncertainty. A later agent rejection is a
submission update, not a retroactive RPC failure. `confirmed` requires adapter evidence; an
adapter must document what establishes it. Do not synthesize confirmation from echoed text.
Completion need not be inferable per submission when several submissions steer one activity;
an adapter must preserve that uncertainty rather than assign the turn's success to every prompt.

When idle, dispatch directly. During active work, steer if the current adapter/agent advertises
that ability; otherwise queue in daemon-acceptance order. Reevaluate support before dispatch.
Serialize the dispatch decision with activity changes so an attach-time capability read cannot
cause a race. Before dispatch, persist the exact prepared agent payload and command identity.
Recovery of the same attempt uses that payload, not newly composed context.

A lost submit reply is recovered by querying or resending **the same** submission ID and input.
If `get` returns `not_found`, resubmitting that same ID remains safe even if the original request
is still in flight. A new ID is a new intent. This guarantees daemon-side deduplication, not
exactly-once agent execution. If relay/agent loss leaves an attempt possibly delivered without
proof, mark the uncertain fields `unknown`; do not automatically send it again. Replaying text
or its absence is not proof. A knowingly unsent queued submission can remain queued, but on agent
replacement it is held until an explicit resume action chooses delivery to the replacement.
SFP v1 exposes cancellation of individual queued submissions and explicit resume of held
submissions, preserving daemon-acceptance order. It does not expose queued-input editing or
reordering. Resume does not authorize resending an attempt whose insertion is unknown. Exact
method shapes, idempotency and dispatch/cancel races remain schema work.

### 7.6 Activity, cancellation and agent replacement

Expose foreground activity as `idle`, `working`, `waiting` or `unknown`, with applicable turn and
question references. Report background work separately; unknown background state is not an empty
list. Foreground idle alone does not authorize cleanup or establish that processes have exited.
Adapter loss yields unknown activity until reconciliation, not a fabricated successful finish.

Cancellation is an explicit request with a defined target: a queued submission, a turn/foreground
activity, or a managed process. It acknowledges that cancellation was recorded/requested, not
that the agent or process has stopped. Confirmed outcomes arrive in the relevant state updates.
Unsupported scopes return `unsupported`; adapters must not silently broaden a cancel request.
Exact cancel methods, idempotency keys and races with completion must be specified before freeze.

`agent_generation_id` changes when the underlying conversation/worker is replaced without
continuity; a main-daemon restart adopting the same conversation preserves it. The generation
change is sequenced through `session.updated`, before new-generation items/questions arrive.
Keep earlier SFP item identities and session sequence; never relabel old items with a new
generation. Reconciliation may reuse an existing item only with a durable identity mapping, not
because text matches. Record a visible discontinuity and reconcile old active turns/submissions
to their known or unknown outcomes. Cancel obsolete questions explicitly. How ACP v1 reload and
ACP v2 replay establish continuity must be exercised by their adapters before freeze.

### 7.7 Questions and competing answers

Permissions and forms are independent question objects. Their tool reference may be null.
`question_id` is never reused, even if an agent reuses its request number. States are `open`,
`answered` and `cancelled`; terminal records retain a reason and, where applicable, the winning
answer ID and delivery state (`pending`, `confirmed`, `unknown`). With no frontend attached an
open question waits, unless daemon policy answers it. There is no frontend-disconnect timeout.
Policy answers follow the same state/event rules as human answers.

`_sill/questions.answer {session_id, question_id, answer_id, answer}` validates the answer and
atomically claims an open question. Persist the winner and delivery intent before returning the
question record. Only the first valid answer wins. Deduplicate answer IDs within the session for
its lifetime: identical retries return the recorded decision/current delivery state, conflicting
reuse returns `id_conflict`. Answer input identity includes both `question_id` and answer content;
reusing an answer ID for another question conflicts even when the answer content is identical.
Another answer to a terminal question returns `stale_answer` with
its resolution. Publish `question.updated` so every observer dismisses or updates its dialog.

Recording the decision does not claim that the agent received it. A lost delivery reply must not
reopen the question; recovery retries delivery only when the relay can deduplicate it. Otherwise
report unknown delivery. Questions from a lost generation are cancelled or retain their recorded
answer with unknown delivery; neither can authorize a new generation's action. Unknown permission
kinds/options cannot be interpreted as allow. A client that cannot safely render a question leaves
it pending or uses an explicitly defined cancellation answer, never a guessed default.

### 7.8 Schema conventions, errors and host operations

Publish JSON Schema draft 2020-12 for every public request, result, notification and error-data
shape. Use named objects instead of positional tuples, and tagged objects with a `type` field
instead of Rust's externally tagged enums. Open string enums preserve unfamiliar values; fields
with authorization or state-machine meaning must also document safe unknown-value handling.
Use decimal strings for counters that can exceed JavaScript's safe integer range, and UTC RFC 3339
strings for timestamps. The examples above do not replace those schemas.

JSON-RPC parse/invalid-request/method-not-found/invalid-params errors retain their standard numeric
codes. Proposed domain errors use numeric code `-32000` and
`data: {code: <stable string>, details?: <typed object>}`. Human `message` text is diagnostic;
clients branch on `data.code`, not English text. Initial domain codes:

| Code | Meaning |
|---|---|
| `incompatible`, `not_initialized` | Handshake failure or initialization required. |
| `not_found`, `gone` | No known resource, or a known resource/token has been removed. |
| `invalid_state`, `unsupported`, `busy` | Operation cannot run in this state, lacks support, or hits a temporary resource conflict. |
| `id_conflict`, `stale_answer` | Idempotency-key conflict or an already resolved question. |
| `invalid_cursor`, `snapshot_expired` | Invalid resume position or expired snapshot lease. |
| `limit_exceeded`, `permission_denied` | Advertised bound exceeded or daemon policy refused the operation. |

Method schemas must define error details and whether failure establishes that no side effect
occurred. A timeout/connection loss establishes no such thing. Unknown error codes display the
message and do not trigger automatic mutation retries. Active work is not itself a `busy` error
for submission: steering/queuing is the settled behavior.

Replace `frontend.request` with individually specified methods grouped under `files.*`,
`uploads.*`, `procs.*`, `registry.*`, `agents.*`, `settings.*`, `policy.*` and `streams.*`, alongside
the existing session, pin, changes and memory groups. Public shapes must not serialize core
configuration/store structs by accident. Inventory every existing suboperation and classify it
as public, replaced, or explicitly internal. Do not exclude file/image access solely because
the shelf uses it; independent clients need equivalent capabilities. Pure presentation helpers
may remain outside the public contract, clearly marked `x-sill-internal` in schema metadata.

Paths identify locations on the selected daemon box, never the frontend host by implication.
Operations must distinguish host paths from a session's merged filesystem paths. Paths carry
lossless bytes plus display text, including for Linux filenames that are not valid UTF-8. Exact
bytes identify the path; display text is presentation only and must never be round-tripped as
its identity. Final field shapes remain schema work. Never silently replace invalid bytes in
an operational path. Binary JSON values use base64 with decoded-size limits; larger uploads use a
specified stream/chunk contract. JSON arrays of byte numbers are not the public representation.

Every list/history operation needs bounded pagination and documented consistency. Every mutation
needs retry semantics: either an idempotency key and retained outcome, a naturally idempotent
operation with explicit preconditions, or an explicit prohibition on blind retry after uncertainty.
Keep/Revert and other host mutations retain their filesystem/precondition checks; a retry token
does not override a changed file. Final method schemas, limits and mutation recovery tables are
required before freeze, not supplied by this first draft.

### 7.9 Streams

Keep separate control and byte-stream connections, including over SSH. File descriptors never
cross SFP. A stream-open result must identify the stream independently of its short-lived,
single-use connection token. The token is bound to the authenticated owner, operation and target;
its lifetime and whether the issuing control connection must remain open must be specified.
Opening a second connection must not accidentally start another daemon.

Before freezing, specify the prelude and success/error framing, token consumption on failed opens,
maximum buffering/backpressure, EOF/half-close, cancellation, disconnect behavior and teardown
for each of PTY, port, login, install and upload streams. PTY resize targets the stream ID rather
than a consumed token; process exit status is distinguishable from transport loss. Stream
reconnection must explicitly say whether it resumes, opens a new stream or is unsupported.
Interactive terminals survive frontend/control or byte-stream disconnection within their session
or explicit lifetime. A frontend can reattach to the same terminal; a disconnect alone neither
ends it nor creates a replacement shell. Reattachment replays a bounded buffer of recent output
and explicitly reports any gap when earlier bytes are unavailable. This is not an unlimited
terminal byte log. Buffer limits, byte offsets, replay/live handoff, terminal lifetime/cleanup
and concurrent attachment rules remain technical specification work. This terminal policy does
not imply that port, login, install or upload streams support resumption.

An agent's terminal display item is not an interactive PTY capability. Raw stream bytes are not
durable session events; any durable status/result is reported through its owning operation.
The existing protocol 6 mechanism in §1 is implementation evidence, not a completed SFP contract.

### 7.10 Implementation order and freeze evidence

1. Review this model and resolve the foundational open schemas; separate relay types before
   changing public wire shapes. Assess the initial relay migration explicitly.
2. Implement initialization, the durable session event model, snapshot/replay, submissions and
   questions as one working slice. Preserve ACP v1 behavior behind the adapter.
3. Build a small Python client from exported schemas immediately, then expand it
   alongside the implementation. It imports no `sill-core` types and reads no store files.
4. Complete the public method inventory, errors, limits and stream contract; migrate shelf and
   CLI to the same public semantics. Mark historical frontend-as-ACP descriptions as superseded.
5. Exercise an experimental, opt-in ACP v2 adapter against real v2 behavior, retaining ACP v1.
   Resolve findings in SFP, then remove the draft suffix only after the conformance suite passes.

Required conformance scenarios include:

- Daemon crash after state/event commit but before publication or reply; replay does not duplicate
  items, appended text, accepted submissions or answers.
- Retained, expired, future and foreign cursors; a paginated snapshot while live writes continue;
  slow-reader disconnect and replacement subscriptions with old notifications queued.
- Duplicate submission IDs with identical and different input; dispatch followed by lost agent
  state; queued versus steering delivery; no automatic resend of uncertain work.
- Two frontends racing different answers; retry of the winning answer; no observers attached;
  agent replacement reusing source request/item numbers.
- Unknown fields/content/enums, an unknown required state event, and an unknown authorization
  meaning; no silent state corruption or permission grant.
- Lossless non-UTF-8 path round-trips, including operations using exact bytes rather than display text.
- Queued cancellation and held-submission resume racing dispatch; preserved acceptance order.
- Terminal reattachment to the same shell, bounded output replay and explicit gaps; ordered
  replay/live handoff and cleanup at the terminal's defined lifetime boundary.
- Socket and SSH behavior, protocol mismatch without automatic restart, stream token expiry,
  transport loss versus process exit, and resource cleanup after stream teardown.

### 7.11 Open choices for the next revision

These are design work, not reasons to reopen §6's product decisions:

- Exact baseline item/content, session/activity, tool, form and attachment schemas; which related
  transitions require a single atomic event; explicit evidence rules for adapter confirmations.
- History retention and paging of terminal records; snapshot/event/message limits and expiry;
  flow control sufficient for a snapshot to catch up under sustained output.
- Exact held-submission resume, targeted cancellation and other mutation methods, including key scope,
  retention and uncertain-outcome recovery outside submissions/questions.
- Exact lossless path field shapes, upload framing and stream lifecycle/token-ownership rules;
  terminal replay bounds/offsets, replay/live handoff, lifetime/cleanup and concurrent attachments.
- Capability names/versions and required error-detail schemas; full classification of today's
  `frontend.request` helpers and schema tooling for the independent Python client.
- Migration of existing stored transcripts to stable IDs, including limits on continuity claims
  when an old transcript lacks source identities. Do not deduplicate by text to fill those gaps.

Session state includes `active_ms` (completed working intervals in milliseconds) and
`active_since_ms` (Unix milliseconds when the current interval began, or null).
The journal accumulates only `working` activity in an open session; waiting for input,
idle, unknown, and ended states stop the clock. Snapshots preserve the total across
frontend reconnects. Older state defaults to zero; past work is not reconstructed.
