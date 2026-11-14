# sill — Sill Frontend Protocol (SFP)

**Implementation in progress, 2026-11-03:** the current control connection uses
`_sill/initialize` with identity **`sfp/1-dev.3`**. This development identity implements the
conversation and host administration surfaces described below; it does not yet claim
stable `sfp/1`. Clients and daemon upgrade together. A mismatch requires explicit replacement;
a failed handshake never authorizes restarting a daemon. Private relay protocol 2
requires a coordinated cutover from older development workers.

## Clean baseline decision (2026-11-03)

Sill currently has one user and disposable development history on every machine. The
compatibility promise starts at stable `sfp/1`, not at any development identity, database
layout or private relay version. Old transcript migration is unnecessary, not deferred.
History/terminal retention remains separate work; discarding old history does not remove
the need for correct new-session recovery, retry identities, snapshots or explicit gaps.

The cleanup removes old control RPC aliases, transcript import, historical schema upgrades,
individual source-receipt fallback and old relay feature negotiation. Incompatible storage
is refused explicitly rather than silently migrated or deleted. Existing complete schemas
and canonical records remain usable, except obsolete source receipts require a clean store.
Legacy inline-upload references are not preserved for retries; start new sessions at cutover.
Active transcript recording remains because summaries and memory still consume it.

For deployment, stop sessions using the old build, then upgrade daemon and clients together
on each machine. Back up state before any reset. Preserve configuration, agent definitions,
credentials and pins; a database also contains pins, notes and Changes/undo records, so deleting
it is not a history-only reset. No automatic data wipe, installation, or daemon restart is
part of this cleanup. Old workers are incompatible with private relay protocol 2.

Stable identity promotion still follows conformance and user testing. ACP v2 remains opt-in;
real-agent validation and the release-contract decision are still outstanding. Historical
protocols, old draft requirements and decisions are archived in
[protocol-history.md](protocol-history.md); they do not override this baseline.

## Transport

Control uses newline-delimited UTF-8 JSON-RPC 2.0 on the same-user Unix socket
`$XDG_RUNTIME_DIR/sill/daemon.sock` (0600 in a 0700 directory). Remote clients use system
SSH to run `sill daemon --stdio`, which bridges to the remote daemon. Request IDs correlate
responses; notifications have no ID. Public methods use `_sill/<group>.<method>`.

The first control request must be `_sill/initialize`, offering exact protocol identities,
a client name/version and optional capabilities. No match returns `incompatible`; other
methods before initialization return `not_initialized`, and repeated initialization returns
`invalid_state`. Successful initialization returns the chosen identity, daemon identity,
negotiated capabilities and limits. A mismatch never authorizes automatic daemon replacement.

Stream-opening methods return a one-use token. Open a separate connection on the same
transport and send `{"_sill_stream":"TOKEN"}\n`, then exchange the method's stream bytes.
Unused tokens expire in 60 seconds; at most 64 wait at once. Descriptors never cross SFP.
Rust control calls default to a 120-second deadline (5 seconds for daemon status/shutdown);
timeout closes the connection and does not establish whether a mutation took effect.

## Conversation and administration

The implemented public conversation surface is `_sill/sessions.attach`,
`_sill/sessions.snapshot`, `_sill/sessions.detach`, `_sill/sessions.submit`,
`_sill/submissions.get`, `_sill/submissions.cancel`, `_sill/submissions.resume`,
`_sill/questions.answer`, `_sill/sessions.control`, `_sill/sessions.cancel`,
`_sill/uploads.create`, `_sill/uploads.get` and `_sill/session.event`, plus `_sill/daemon.schema`.
The schema command exports the concrete records and method shapes. Counters are decimal strings;
records/events use named objects and `type` tags. `scripts/sfp_client.py` is an independent Python
consumer and schema validator (install `scripts/requirements-sfp.txt`).

Conversation changes, public sequence, source deduplication and runner state commit before SFP
publication and relay ACK. Replay retains a contiguous suffix of at most 4096 events and 16 MiB
per session. Older cursors automatically select a replacement snapshot; foreign/future cursors
remain errors. Canonical records, submission/answer retry identities and source deduplication
survive replay compaction. Source receipts coalesce adjacent sequence ranges without filling unseen gaps. This bounds replay storage, not the retained conversation or identity
ledgers; those remain proportional to session history.

Snapshots use private, automatically deleted SQLite temporary files with a 2 MiB cache per
lease instead of retaining the entire conversation in memory. Leases are connection-bound, expire after
60 seconds without a successful page read, and have a 30-minute absolute lifetime. Each page
returns its renewed `expires_at`. There are at most eight per connection and 64 daemon-wide, 1 GiB serialized data per snapshot and 2 GiB aggregate.
Replacement capture reserves space alongside the previous lease until it succeeds. If that
exceeds the aggregate quota, detach the old subscription to release its lease before attaching again.
Pages contain at most 128 ordinary records and approximately 1 MiB of record JSON. Larger
records (up to 64 MiB) use consecutive `fragment: {record, offset, json, last}` pages with empty
`records`: concatenate `json` through `last`, validating the same record identifier and decimal
UTF-8 byte offset, then decode one record. No ordinary record may interrupt a fragment sequence.
Fragments contain at most 256 KiB of UTF-8 before JSON escaping. Page tokens are opaque to clients.
The Rust and independent Python clients reconstruct fragments. Capture still holds the store
lock while copying the history, and clients currently materialize the whole resulting view.

Control messages are limited to 8 MiB including newline, prompt text to 1 MiB JSON-encoded,
and initialization must arrive within ten seconds. Outbound connection queues have a 16 MiB
byte budget as well as a message-count bound, with a 256 MiB daemon-wide queued-byte budget.
A slow consumer is disconnected and resynchronizes. An oversized committed live event also
closes its connection; a reconnect selects a segmented snapshot instead of replaying that event.
Agent text/thought output splits into items of at most 256 KiB.

ACP v1 queues submissions during activity; it does not advertise steering. Prepared payload and
relay command ID are persisted before sending. A successful ACP prompt result establishes
insertion/completion (or the agent's reported cancellation). Prompt RPC errors report uncertainty.
Replacement holds unsent submissions and marks possibly dispatched work unknown. Old transcript rows are not imported into SFP. A session without canonical SFP state starts
a new empty conversation view; historical text never establishes continuity. Permission/form answers persist the winning answer
and delivery intent before sending. Relay protocol 2 requires outcome evidence; its resolution confirms handoff to the ACP question
handler, without claiming that the remote agent acted on it. Older relay versions are refused.

Uploads are immutable session-scoped objects with client IDs and bounded base64 data. Inline
attachments currently support images; file attachments reference uploaded objects. Upload IDs
retain content identity independently of their staged jail file. The daemon stores an inline
attachment as an upload when it accepts the submission (a reserved `sill-inline-` identity), so
submission records reference attachment bytes and never carry them; a frontend fetches what it
displays with `_sill/uploads.get`. Pre-baseline attachment identities are not migrated.
An uploaded PNG, JPEG, GIF or WebP reaches the agent as an image block when the agent advertised
image support; any other upload, and every upload to an agent without it, is a staged file.

Inline upload IDs now hash the complete submission identity and index under the reserved
`sill-inline-` prefix. Public upload creation rejects that prefix; retries of previously accepted
submissions deterministically use the same hashed references. Session uploads are bounded at 4096 objects and
256 MiB of decoded bytes; identical retries still work at the limit. Shelf upload caching and
some presentation duplication remain UI optimizations rather than compatibility blockers.

A login method in `session.authentication` with `api_key` set (the provider the agent names,
from ACP `_meta["api-key"]`) takes a key the person types: send it as `api_key` with the
`authenticate` control. The daemon saves it readable by the person only, outside every folder a
jail sees (`~/.local/state/sill/keys/<agent>/<method>`), and the relay reads it from there when the
`authenticate` request goes, in both shapes adapters read (`{"api-key": key}`, then
`{"api-key": {"apiKey": key}}`). It is never journaled or logged. When the agent asks again (a new
agent process), a saved key for an offered method answers it without `authentication` being set;
refused once, the login card shows with the agent's reason. `agents.reset` removes saved keys.

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

These baseline methods need no optional capability. Metadata is an explicit public record,
including settings, pins and decimal-string Unix-second timestamps; it exposes no ACP session
identity or shim socket. `policy_changed` is true when a running session's policy file changed
after its jail was built; stopping and starting the session applies it. Every operation uses an exact session ID. The CLI resolves unique
prefixes/titles locally and rejects ambiguity. Shelf session listing and lifecycle writes, the
CLI and the Python client's `list/create/get/update/start/stop/end/checkpoint` use this surface.
Public `update` uses a named, `type`-tagged change; unknown change kinds are rejected.
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
path are accepted. Pin storage and jail setup preserve non-UTF-8 bytes. Existing UTF-8 TEXT rows remain readable; new non-UTF-8 pins use BLOB identities. Changes history and kept journals also preserve exact new paths, including newlines. Previously lossy historical values cannot be reconstructed.

Independent lifecycle conformance (with `jsonschema` installed in a venv):

```sh
SILL_SFP_PYTHON=/path/to/venv/bin/python cargo test -p sill-shelf --no-default-features --test recovery independent_python_client -- --ignored
```

The fixture supplies a scripted ACP agent; Python creates the session, submits work, reconnects
with its saved view after the main daemon is killed, answers the recovered question and ends
the session. Every Python request/result/event is checked against the exported schema, without
negotiating internal APIs or reading the database. Rust tests also cover create rollback,
conflicting/repeated IDs, deletion tombstones, paging and lifecycle metadata across restart.

Public terminals are implemented too (2026-11-03):

| Method | Request | Result |
|---|---|---|
| `_sill/terminals.open` | `{session_id?, label?, argv?, env?, cwd?, cols, rows}` | terminal record |
| `_sill/terminals.list` | `{session_id?}` | `{terminals}` |
| `_sill/terminals.get` | `{terminal_id}` | terminal record |
| `_sill/terminals.attach` | `{terminal_id}` | `{token}` |
| `_sill/terminals.resize` | `{terminal_id, cols, rows}` | `{}` |
| `_sill/terminals.cwd` | `{terminal_id}` | `{cwd}` |
| `_sill/terminals.close` | `{terminal_id}` | `{}` |

The daemon owns each PTY master. A terminal runs in a session's jail (`session_id`) or on the
daemon host; empty `argv` runs the login shell; `cwd` is a host path record in the jail's or
the host's namespace. `label` (at most 128 bytes) is the frontend's own name for finding the
terminal again; the shelf uses `shell` in a session and `host:<session id>` on the host.

`attach` returns a one-use byte-stream token (see Transport below). The stream carries, in order, the replay
buffer — the last 256 KiB of output, trimmed to start on a line — and then live output; bytes
written to it are keystrokes. The replay is taken and the viewer subscribed under one lock, so
nothing is missed or doubled between them. `discarded_bytes` in the record counts output no longer
kept: nonzero means a replay starts partway (the shelf says so in the terminal). Raw bytes are
unframed; the stream's end means the viewer detached, fell behind, or the terminal exited or was
closed, and `get` tells which (`state`, `exit_code`). Closing the stream only detaches. At most
8 viewers attach to one terminal; all receive output, any may type, and the last resize wins. A
viewer whose 256-chunk queue fills is cut off rather than slowing the terminal; it attaches
again and gets the replay.

A terminal lives until its program exits, its jail stops, `close` hangs it up (the daemon
drops the master), its session is deleted, or the daemon leaves: the daemon holds the master,
so a daemon restart still ends every terminal. An exited terminal remains readable for 10
minutes; the daemon holds at most 64 terminals. `exit_code` is known for host terminals only.
`open` must not be blindly retried after connection loss (list by label instead); `close` is
idempotent in effect (a repeat returns `not_found`).

Transport bounds: messages are limited to 8 MiB; a connection must initialize (or claim a
stream) within 10 s; the daemon serves at most 256 connections at once (control and streams)
and refuses more; at most 64 stream tokens wait unclaimed, each for 60 s, dropped on the next
token and by the minute sweep without a thread of their own.

Public host streams are implemented (2026-11-03):

| Method | Request | Result |
|---|---|---|
| `_sill/streams.port` | `{session_id, port}` | `{token}` |
| `_sill/agents.login` | `{agent_id, method_id, operation_id?}` | `{token, operation}` |
| `_sill/agents.reset` | `{agent_id}` | `{removed, stopped}` (no stream) |
| `_sill/registry.install` | `{entry_id, version, source, allow_unreviewed?, operation_id?}` | `{token, operation}` |
| `_sill/registry.list` | `{after?, limit?}` | `{entries, next}` |

These baseline methods need no optional capability. Login selects a configured agent on the daemon box, never a frontend
command. The agent validates the method during login; an unsupported method is a streamed failure.
Install selects the exact ID, version and source (`snapshot` or `live`) from the daemon's current
registry, retaining its reviewed lockfile/distribution. This selector is not a content hash of a
live index entry. Missing selectors return `not_found`; unknown sources or malformed identifiers
return invalid-params; unreviewed entries require `allow_unreviewed: true` (`permission_denied`
otherwise), and an unavailable platform returns `unsupported`, all before starting a job.

`_sill/agents.reset` returns an agent to how a fresh install starts: first it stops the agent's
running sessions (a running agent holds its login and writes to its home; the conversations stay
and start again when used), then it removes the agent's sill-owned home — login, settings,
history, memory — the copies in this daemon's ghost sessions and its saved API keys. `removed`
counts the files, `stopped` names the sessions. The install, its definition and the host's own
files are untouched; an unknown agent returns `not_found`.

Registry listing uses exclusive entry-ID keyset pagination with 1–128 entries (default 64),
at most one newer version per entry and a 4 MiB entry payload budget per page. Pages reflect
live registry contents, so concurrent refresh can change later pages. Records expose name,
version, source, description, links, icon availability, builtin-agent mapping and daemon-platform
installation facts, without raw package manifests or executable commands. Pages also report
the live index fetch time as decimal Unix seconds. Shelf builds its presentation from these
public registry and agent methods.

Claim tokens as described under Transport: one use, on a new transport within 60 seconds. Port streams
carry raw bidirectional TCP bytes to a running session jail; no jail is started implicitly.
Port zero is invalid; an absent session returns `not_found`, a stopped jail or failed connection
returns `invalid_state`. Closing the stream closes the connection. Input EOF half-closes the
TCP direction while the other direction can finish. No replay or reattachment is available.

Login/install streams carry UTF-8 newline-delimited tagged JSON, described by
`streams.host_progress.event` in the exported schema:

- `{type: "status", text}` or `{type: "stderr", text}` for login progress.
- `{type: "output", text}` for installer output.
- `{type: "completed", agent_id}` establishes success. Read current agent configuration separately.
- `{type: "failed", code: "operation_failed", message}` establishes failure; filesystem changes
  or login side effects may already have occurred. Unknown failure codes never authorize retry.

Each text/message is truncated at a UTF-8 boundary to 16 KiB. At most four login/install jobs
run concurrently; a second job of the same kind for the same agent/entry returns `busy`, and
an exhausted job budget returns `limit_exceeded`. Progress writes have a one-second timeout;
a stalled reader loses its stream while the job continues. A job reserves its slot until it
finishes, including after disconnect. Login retains its existing agent-response deadlines
(30 seconds for initialization, ten minutes for authentication); installers have no new overall
deadline. Cancellation and reconnect/replay are unsupported for these two job streams.
Closing or never claiming their stream **does not cancel the job**. EOF without a terminal event,
a lost token/reply, or daemon death does not establish the outcome. Login/install callers that
retained an `operation_id` recover it through `_sill/operations.get` or an identical opening
request: the latter returns the receipt with `token: null`, never a duplicate job. Without that
identity, inspect host state before an explicit retry. Port opens must not be blindly retried.
Stream tokens and active jobs are in memory; durable receipts survive daemon restart and unfinished
jobs become `unknown`. Schema/argument
validation, selector/policy refusal and job/token-limit refusal start no job. A failure after job
start or a lost reply provides no such guarantee. The Python client's `registry`, `login` and
`install` commands exercise public selectors and validate every progress event against the schema.

The public APIs below cover host/session administration, filesystem paths, Changes, memory,
settings, policy and managed processes. There is no internal frontend RPC surface.

The experimental native ACP v2 adapter is implemented behind `experimental_acp_v2 = true`, with
ACP v1 retained as the default. See [native behavior and recovery evidence](sfp-operations.md#experimental-native-acp-v2).
This milestone remains `sfp/1-dev.3`: real third-party v2 interoperability, historical replay
after relay replacement, large-session performance review and user acceptance testing remain
before a stable freeze. Snapshot capture still copies under the store lock; canonical history
and idempotency identities remain proportional to retained sessions, and native v2 reconstruction
currently caches retained source payloads. The implemented quotas are intentional bounds, not a
promise of unlimited history or automatic eviction of retry identities.

### Additional development public APIs (2026-11-03)

`_sill/changes.changed` is the public invalidation notification for Changes views.
`_sill/files.image` provides bounded session image reads; `_sill/policy.denied`,
`_sill/policy.remember_denied`, and `_sill/policy.allow_host` expose network permission state and explicit decisions.

These baseline APIs need no optional capability. The exported schema is authoritative for their
named request/result records. They do not make the development identity a stable `sfp/1` release.

| Methods | Contract |
|---|---|
| `_sill/files.home`, `_sill/files.metadata`, `_sill/files.list` | Exact daemon-host paths; live directory keyset pages (1–128); byte identities, display text ignored. |
| `_sill/files.read`, `_sill/files.write` | Base64 regular-file data; reads use decimal byte offsets and chunks up to 1 MiB; writes atomically replace up to 1 MiB. Reads refuse symlinks. |
| `_sill/settings.get`, `_sill/settings.patch` | Explicit settings record and tagged changes; unknown change kinds rejected; at most 64 changes and 1 MiB input. |
| `_sill/settings.instructions.get`, `_sill/settings.instructions.set` | Global instruction text, at most 1 MiB input. |
| `_sill/policy.list`, `_sill/policy.get` | Policy names and TOML policy document with desktop flags. |
| `_sill/procs.list`, `_sill/procs.tail`, `_sill/procs.stop`, `_sill/procs.dismiss` | Session managed processes; bounded live pages, tail up to 10,000 lines; stop requests termination, not proof of exit. |
| `_sill/pins.list`, `_sill/pins.add`, `_sill/pins.remove`, `_sill/sessions.pin`, `_sill/sessions.unpin` | Global or exact-session pins; live byte-order keyset pages, 1–128 entries; global add canonicalizes. |
| `_sill/agents.list`, `_sill/agents.get`, `_sill/agents.save`, `_sill/agents.remove`, `_sill/agents.available` | Explicit configured-agent definitions, ID keyset pages (1–128); save/remove user overrides; availability resolves the configured command on the daemon. |
| `_sill/agents.defaults.get`, `_sill/agents.defaults.set` | Named agent default controls; unknown agents refused. |
| `_sill/registry.refresh`, `_sill/registry.installed`, `_sill/registry.uninstall`, `_sill/registry.icon` | Resolve daemon registry selectors; uninstall verifies installed version/source; icons return bounded base64 bytes. |
| `_sill/memory.list`, `_sill/memory.text`, `_sill/memory.search` | Live memory listing/search and paged show/transcript/export/recall text; search currently limited to 500 results. |
| `_sill/memory.notes.list`, `_sill/memory.notes.add`, `_sill/memory.notes.remove`, `_sill/memory.refresh` | Paged notes and explicit mutations; additions are not safe to retry blindly. |
| `_sill/changes.list`, `_sill/changes.diff`, `_sill/changes.keep`, `_sill/changes.revert`, `_sill/changes.undo`, `_sill/changes.git-revert`, `_sill/changes.history` | Named Changes records and exact paths; existing filesystem preconditions still apply; consult schemas for paging and selectors. |
| `_sill/operations.get` | Durable login/install receipt recovery; see [operation recovery](sfp-operations.md). |

Unless a method explicitly offers a durable retry identity, a lost mutation reply is uncertain:
read back the target before deciding whether to retry. Desired-state writes can overwrite a
concurrent edit; a retry is not permission to do so. Shelf no longer blindly retries mutations
through its frontend data connection. Most public data responses have a 4 MiB payload ceiling.
The public files methods identify the daemon **host** namespace; they do not yet provide a
complete merged-session filesystem API. Memory text uses connection-bound immutable disk leases built by streaming formatters against a separate read-only SQLite snapshot; subsequent pages seek within the spool. `_sill/memory.text.close` releases a lease early.

## Compatibility and errors

**Capability names and versions (settled for sfp/1, 2026-11-03).** A capability name is
`namespace.feature`: at least two dot-separated parts of lowercase ASCII letters, digits and `_`,
at most 64 bytes. The `sill.` namespace is the daemon's own. Versions are integers from 1 and
cumulative: version N keeps every guarantee of version N−1. In `_sill/initialize` the client
offers each capability at the highest version it implements; the daemon answers each one it has
at min(offered, its own) and omits unknown names and offers of 0. Only answered capabilities may
be used on that connection. A method that needs a missing capability returns `unsupported` with
`details.capability`. The exported schema lists the daemon's capabilities under
`capabilities.offered` and each method's requirement under `methods[].capabilities`.

The sfp/1 baseline needs no optional capability. The current capability registry is empty;
`sill.internal` and the `_sill/internal/*` methods have been removed.

Agent abilities (`steering`, `images`) are not handshake capabilities: they are reported in
session state, may change with the agent, and use the same open-map rule (unknown keys ignored).
Native ACP v2 is how the daemon talks to an agent; it changes nothing on the SFP wire and so is
not an SFP capability either.

**Extension rule after the freeze (decided 2026-11-03).** The identity remains `sfp/1` for the
lifetime of v1; there are no `sfp/1.x` minor versions. A compatible addition is a new method,
notification, optional request/result field, enum variant or error detail that an old peer can
ignore or never sees. Each addition is advertised as a named capability (`name: version`) in
`_sill/initialize`; a daemon uses it toward a client only when the client offered it, and a
client calls it only when the daemon returned it. A missing capability disables that feature;
it is not a protocol mismatch and never triggers the explicit-replacement flow, which is
reserved for a protocol-identity mismatch (`sfp/1` vs `sfp/2`). Changing the meaning of an
existing field, making an optional field required, or removing anything requires `sfp/2`.
Optional SFP wire features a build or platform can disable need capabilities too.
An experimental agent adapter does not itself change the SFP wire contract. The daemon build version and protocol revision in `daemon.status` are
diagnostics only; clients must not gate behaviour on them. Example: durable `operation_id`
receipts for Changes mutations would be a capability such as `changes.operation_id: 1`.

JSON-RPC parse/invalid-request/method-not-found/invalid-params errors retain their standard numeric
codes. Domain errors use numeric code `-32000` and
`data: {code: <stable string>, details?: <typed object>}`. Human `message` text is diagnostic;
clients branch on `data.code`, not English text. Domain codes:

| Code | Meaning |
|---|---|
| `incompatible`, `not_initialized` | Handshake failure or initialization required. |
| `not_found`, `gone` | No known resource, or a known resource/token has been removed. |
| `invalid_state`, `unsupported`, `busy` | Operation cannot run in this state, lacks support, or hits a temporary resource conflict. |
| `id_conflict`, `stale_answer` | Idempotency-key conflict or an already resolved question. |
| `invalid_cursor`, `snapshot_expired` | Invalid resume position or expired snapshot lease. |
| `limit_exceeded`, `permission_denied` | Advertised bound exceeded or daemon policy refused the operation. |
| `operation_failed` | A durable host operation (login, install) recorded a failure. |

**Error details (settled for sfp/1, 2026-11-03).** Only these codes carry `details`; every other
code carries none in sfp/1. The exported schema lists them under `errors.details`, each with its
shape and whether it is always present. Clients validate a listed code's details and ignore
details of unlisted or unknown codes.

| Code | `details` | Present |
|---|---|---|
| `incompatible` | `{supported_protocols: [string]}`: the identities the daemon supports. | Always |
| `stale_answer` | `{question}`: the question record as resolved. | Always |
| `limit_exceeded` | `{scope, maximum?}`: `scope` is `request` (send less), `result` (ask for a smaller page), `record` (one stored record is too large for this method; paging cannot help) or `capacity` (a daemon resource is full; release one or wait, an unchanged repeat fails again). `maximum` is the bound as a number in the unit the message names, where one exists. | Always |
| `unsupported` | `{capability}`: the capability the connection did not negotiate. | Only when a capability is missing; refusals about the target (platform, policy, agent) carry none |

A detail shape may gain optional fields within sfp/1 and clients ignore unknown fields. Adding
details to another code is a compatible addition under the extension rule above.

Session state includes `active_ms` (completed working intervals in milliseconds) and
`active_since_ms` (Unix milliseconds when the current interval began, or null).
The journal accumulates only `working` activity in an open session; waiting for input,
idle, unknown, and ended states stop the clock. Snapshots preserve the total across
frontend reconnects. Older state defaults to zero; past work is not reconstructed.


### Public host data and memory details (development implementation)

The exported schema now describes `files.home/metadata/list/read/write/image`,
`settings.get/patch`, `settings.instructions.get/set`, `policy.list/get/denied/remember_denied/allow_host`,
`procs.list/tail/stop/dismiss`, and `memory.list/text/search/notes.list/notes.add/notes.remove/compact/forget/refresh`.
They need no optional capability. Shelf uses the public file, settings, instructions,
network-denial, process and memory operations; CLI memory readers use the same public contract.

- `files.*` paths name the daemon host. `files.image` instead names an absolute path in the
  identified session's filesystem view and only accepts image extensions. Directory pagination
  is live, ordered by exact path bytes; `after` is an exclusive direct-child path. Pages default
  to 64 entries and allow 1–128. Metadata describes the link itself and carries an absolute link
  target separately. Reads refuse symlinks and special files, including FIFOs.
- File and image reads return at most 1 MiB as base64. Preserve the returned `revision` for later
  chunks; inode, device, size, modification and change timestamps detect changes and produce
  `invalid_cursor`. This detects edits rather than providing an immutable file snapshot. Whole-file
  writes atomically replace at most 1 MiB and are not blindly retried after uncertainty; inspect
  file contents first. Larger writes need a future upload-to-file contract.
- Settings patches contain a bounded list of named, typed changes. Unknown change kinds are
  rejected. Settings updates serialize through a daemon mutex. Instruction writes are capped
  at 1 MiB. Network-host changes preserve daemon validation and apply to agent defaults.
- Process lists are live, name-ordered pages of 1–128 entries. A process's `cwd` identifies its
  session filesystem, while the enclosing request supplies `session_id`. Process stop targets one
  explicit nonempty name; listing state resolves uncertainty. Tails allow up to 10,000 lines.
- Memory lists contain ended non-ghost sessions, ordered by immutable identity. Notes are live
  pages ordered by their exclusive row cursor. Notes are removed by exact timestamp and text;
  note creation must not be automatically retried. Search pages cover at most the best 500 hits.
  Search requests are capped at 4096 bytes and pages at 128 hits.
- `memory.text` renders `show`, `transcript`, `export` or `recall` from an immutable disk spool,
  built once through streaming formatters over a separate read-only SQLite snapshot. It does not
  hold the daemon store mutex while exporting. Pages contain at most 1 MiB of UTF-8 text; later
  offsets carry the returned `snapshot_id` and SHA-256 `fingerprint`. Live edits cannot alter an
  existing snapshot. Leases belong to their control connection, expire after 60 idle seconds
  (extended on valid page reads, up to 30 minutes total), and are closed on disconnect or by
  `_sill/memory.text.close`. At most four leases per connection and 32 globally consume at most
  512 MiB of anonymous disk files, with 256 MiB per file and a 30-second construction budget.
  Individual legacy records above 64 MiB return `limit_exceeded`. Temporary files are unlinked
  immediately, so failures and daemon crashes cannot leave exported content on disk.
- Host data responses are bounded at 4 MiB. Policy names and network denial lists are live keyset pages of at most 128 entries;
  denial cursors order unique `(kind, host)` pairs across denied, acknowledged and quiet hosts.
  Logs are scanned with bounded line buffers and bounded candidate sets. Oversized individual
  records return `limit_exceeded`.

All unknown errors, timeouts and disconnects forbid automatic mutation retries. The schema exports
JSON-RPC error records, stable domain codes, incompatibility details and stale-answer resolution.

Additional public method names in this development revision: `_sill/files.find`, `_sill/memory.compact`, `_sill/memory.forget`, `_sill/repos.changed_lines`, `_sill/repos.head`, `_sill/repos.root`, `_sill/repos.status`, `_sill/repos.summary`, `_sill/sessions.delete`, `_sill/sessions.log`, `_sill/sessions.reset`.

`_sill/daemon.status` reports the development protocol identity, RFC 3339 startup time and exact daemon socket path. `_sill/daemon.shutdown` requests daemon exit; it is never automatically retried after uncertainty.

### Public-only clients and server

Shelf, CLI and line mode use only public methods. The internal dispatcher, legacy
conversation subscriptions, schema entries, and `sill.internal` negotiation have been removed.
Unknown methods return JSON-RPC method-not-found; they cannot fall back to an internal handler.
New functionality must have explicit public schemas and bounds.

### Development milestone validation

The headless workspace suite covers persistence, relay adoption, login, paths, queues, snapshots,
public administration and native ACP v2 SDK exchanges. The full GTK frontend also compiles.
Run `cargo test --workspace --no-default-features` and `cargo check -p sill-shelf`.

With Python `jsonschema` installed (set `SILL_SFP_PYTHON` to that environment's interpreter),
`cargo test -p sill-shelf --no-default-features -- --ignored` runs the independent Python
administration, Changes, host stream and lifecycle/recovery scenarios plus real OpenSSH
control/terminal reconnection. The SSH test needs `ssh`, `sshd` and `ssh-keygen`; it uses only
throwaway keys, a loopback listener and temporary daemon state, and shuts them down afterwards.
`python3 -m unittest discover -s scripts -p test_sfp_client.py` checks client reconstruction,
including fragmented UTF-8 records and safe handling of unknown state events.

Daemon and frontend binaries must be rebuilt together for this development identity. A failed
handshake does not automatically stop or replace an existing daemon. No application installation,
running-daemon replacement or repository commit is part of this source change.
