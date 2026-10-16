# macOS Codex Desktop IPC Probe Report

Independent probe. Topology: **official Codex Desktop** ⟷ **standalone Python harness** over the local
IPC socket. No Tuvis, no PR #117, no Codex-as-test-client, no queue, no headless app-server.
Tags: **OBSERVED** measured here · **CODE** read from this build's `app.asar` · **INFERRED** ·
**UNKNOWN** not established.

---

## 1. Executive Summary

**结论：PASS**

macOS ships the same Desktop IPC control plane Windows has. Endpoint and transport differ; the protocol
above them does not. A hand-written client connected, initialised, discovered the thread owner,
subscribed to thread state, injected a turn that landed in the Desktop's own persisted conversation,
observed a live approval request, and decided it remotely — approve **and** decline — with the Desktop
honouring both.

**Architecture A：SUPPORTED** — one Protocol/Control Layer, a swappable Transport Layer.

Two corrections to the Windows reference. (a) The `clientType` literal `desktop` is **absent from every
bundle** here (`CODE`, 0 hits) — treat it as an opaque label, never a constant to match. (b) The Windows
note's `availableDecisions` field **does not exist** here (0 hits); the accepted decisions are a fixed
client-side union, so reading the set off the payload will be wrong.

The largest compatibility fact is not the socket but the **per-method `version` gate**: every request
and broadcast carries a `version` integer compared against a built-in method→version table and silently
dropped on mismatch. It is mandatory, and the likeliest cause of a bare `no-client-found`.

---

## 2. Environment

| Item | Value | Tag |
|---|---|---|
| macOS | 27.0.1 (build 26A434), arm64 | OBSERVED |
| Codex Desktop | `/Applications/ChatGPT.app`, bundle id `com.openai.codex` | OBSERVED |
| Version / build | `26.1013.31256` / `20586` (Electron `Codex Framework` 155.0.8059.80) | OBSERVED |
| Main process | pid 2920, started 17:31:24; IPC socket mtime 17:31:29 | OBSERVED |
| `app.asar` | 585,782,775 B unpacked. Router+client `/.vite/build/src-DsLVGakC.js`; endpoint resolver `application-network-startup-DB1VeU3v.js`; stream state `main-CLu1A25j.js` | OBSERVED |
| Probe harness | Python 3.9.6, raw `AF_UNIX` sockets (deleted after) | OBSERVED |
| `clientType` used | `probe` — accepted verbatim, no validation | OBSERVED |
| Date | 2026-10-16 | — |

Paths redacted: account `<user>`, workspace `<ws>`. No token, cookie, key or conversation content
appears below.

---

## 3. POC Results

### POC-1 — IPC Connect + Initialize: **PASS**

- **Endpoint / transport** `~/.codex/ipc/ipc.sock` — OBSERVED via `lsof -U` on the Desktop main process
  (9 descriptors bound) and `stat`. Unix domain socket, `SOCK_STREAM`, `srw-------` (0600), uid 501;
  parent `~/.codex/ipc` is `drwx------` (0700).
- **Frame** 4-byte LE `uint32` length + UTF-8 JSON, confirmed by decoding live traffic; ceiling
  268435456 B (`CODE`: `Mc=268435456`; receiver rejects `l===0 || l>Mc`).
- **Initialize** needs no `version` (the router intercepts it). Request
  `{type:"request", requestId:"<uuid>", method:"initialize", params:{clientType:"probe"}}` → response
  `{type:"response", requestId, resultType:"success", method:"initialize", handledByClientId:"<uuid>",
  result:{clientId:"<uuid>"}}`. The returned `clientId` is a fresh `randomUUID()` per socket (`CODE`:
  `registerClient`); re-initialising the same socket returns the same id.

**Endpoint dynamism (check A).** The *path* is fixed per user; the *file* is recreated — socket and
directory both have mtime `17:31:29` while the process started `17:31:24`, so it is created at launch
and unlinked/recreated each launch (`CODE`: stale-socket unlink, `chmodSync(sock,0o600)`); not per-PID,
not instance-named. The legacy fallback `os.tmpdir()/codex-ipc/ipc-<uid>.sock` exists in code (`CODE`)
but **is absent here** (OBSERVED). Try `$CODEX_HOME/ipc/ipc.sock` (default `~/.codex`) first, then fall
back — the reverse of Farfield, which knows only the tmpdir form.

### POC-2 — Thread Stream State: **PASS** (`following` is a hard precondition)

A/B on the live owned thread:

| Phase | Action | `thread-stream-state-changed` frames |
|---|---|---|
| A | observe 14 s, no declaration | **0** |
| B | broadcast `thread-stream-following-changed {following:true}`, observe 14 s | **1** — snapshot, targeted at the probe |

Frame received: `{type:"broadcast", method:"thread-stream-state-changed", version:11,
sourceClientId:"<owner>", targetClientIds:["<probe>"], params:{conversationId, hostId:"local",
change:{type:"snapshot", revision:209, conversationState:{…48 keys…}}}}`.

- `change.type` ∈ `{snapshot,patches}`; patches carry
  `{baseRevision,revision,patches:[{op,path[],value?}],acceptedTextChanges?}` and are dropped when
  `baseRevision` ≠ the follower's current revision.
- **No `streaming`/`isStreaming` field** (`CODE`, absence verified) — streaming is local client state;
  observers use `turns[].status === "inProgress"` or `threadRuntimeStatus.type` ∈
  `{idle,active,systemError,notLoaded}` (measured `idle → active → idle` across an injected turn).
- **`conversationState.turns` stayed `[]` even with 5 real turns.** Content arrives via the canonical
  `turnHistory` instead: `entitiesByKey` held exactly 5 turns, `islands[0].entries` 5, both boundaries
  `exhausted`, `isComplete: true`. Reading `turns` alone makes a populated thread look empty — a real
  trap for a remote client.
- Pending work surfaces **only** as `conversationState.requests[]`, entries `{id, method, params}` — the
  sole wire surface for approvals and user-input requests (`CODE`). `thread-stream-following-status-requested`
  (v1) is the owner asking peers "who already follows this?".
- Primitive: **re-broadcasting `following:true` while already a follower makes the owner send a fresh
  targeted snapshot** (OBSERVED).

### POC-3 — Owner Discovery: **PASS**

```
thread-owner-discovery → params {hostId:"local", conversationId:"<tid>"}  version 1
                       ← resultType "success", handledByClientId = <owner clientId>,
                         result {"supportsUntrustedAppInput": true}
```

| Case | Result | Tag |
|---|---|---|
| Live open thread, `hostId:"local"` (also when targeted at that owner) | owner `420dae40-…`, same client that broadcasts `following`/stream state | OBSERVED |
| Same thread, `hostId:"durable"` | `no-client-found` | OBSERVED |
| Thread id nobody holds | `no-client-found` | OBSERVED |

1. The open thread **has** an owner, obtainable from IPC — `threadId → ownerClientId` works.
2. Several threads under one owner — **UNKNOWN**: only one thread existed in this profile.
3. Owner across Desktop restart — **UNKNOWN directly** (app not restarted; that would disturb live
   work). **INFERRED, high confidence:** `clientId` is a fresh `randomUUID()` bound to the socket, so a
   restart necessarily yields a new `ownerClientId`; never cache an owner across a Desktop restart.
4. Invalidation — **OBSERVED and explicit**: `client-status-changed` (v0) goes to all peers on every
   connect/disconnect with `params {clientId, clientType, status:"connected"|"disconnected"}` (both
   states captured for a transient client). On an owner disconnect the follower side drops it from all
   follower sets and fails pending waits with `no-client-found: thread stream owner changed`;
   `ipc-connection-reset` (v1) is the companion signal.

### POC-4 — Start Turn: **PASS**

Request: `thread-follower-start-turn`, v2, `targetClientId` = owner, params
`{conversationId, turnStart:{request:{threadId, input:[{type:"text",text:"Please reply with exactly:
TUVis macOS IPC probe OK",text_elements:[]}], clientUserMessageId}, context:{}}}`.
`threadId` must equal `conversationId` or the owner rejects it.

Response in 0.1 s: `resultType:"success"`, `method:"thread-follower-start-turn"`,
`handledByClientId:"<owner>"`,
`result.result.turn = {id:"01a14414-…", rootTurnId, items:[], itemsView:"notLoaded", status:"inProgress"}`.

Confirmation came from the **Desktop's own persisted thread**, not the IPC reply:

| Check | Result |
|---|---|
| Turn id matches the IPC response | yes — `01a14414-…` in both |
| User message / assistant reply in the same thread | yes / yes |
| Reply text | `last_agent_message = "TUVis macOS IPC probe OK"`, exactly as instructed |
| Duplicate turns | exactly **1** turn carrying the probe text |
| `threadRuntimeStatus` over IPC | `idle → active → idle`; `duration_ms` 99771 |

No shell command ran for this POC; the message was inert and the sandbox read-only.

### POC-5 — Command Approval: **PASS** (via the permissions-request variant)

An approval request **was** triggered, observed from IPC, and decided remotely both ways. Natural
failure mode: telling the agent to run a command and *report the exit status* produced a read-only-sandbox
denial (`exit 1`) and **no** approval request — the path opens only when the agent requests escalation.
Observed pending request (`conversationState.requests[0]`):

| Field | Value |
|---|---|
| `id` (`requestId`) | **`12` — integer** (second run `18`) |
| `method` | `item/permissions/requestApproval` |
| `params` | `{threadId, turnId, itemId, environmentId, startedAtMs, cwd, reason, permissions}` |
| `permissions` | `{network:null, fileSystem:{read:null, write:["/tmp/mac-ipc-probe"], entries:[…]}}` |
| `availableDecisions` | **absent** |

Decision call: `thread-follower-permissions-request-approval-response` (v1) with
`params {conversationId, requestId, response:{permissions, scope:"turn"}}` → `{"ok":true}`.

| Direction | `response.permissions` sent | Result |
|---|---|---|
| **Approve** | requested permissions echoed back | Turn resumed, command ran, file created, agent confirmed |
| **Decline** | `{}` (deny-all) | File **not** created; agent reported the permission was not granted |

**On `availableDecisions` (check F).** The string occurs **0 times** in every bundle here (`CODE`) —
nothing on the wire advertises an allowed set, so the Windows `availableDecisions: cancel` vs actual
`decline` is best explained as another build or method. The accepted decisions are a fixed client-side
union — `"accept" | "acceptForSession" | "decline" | {acceptWithExecpolicyAmendment:{…}} |
{applyNetworkPolicyAmendment:{…}}` — with `"decline"` a first-class member and the value the Desktop's
own auto-deny uses. Never derive the set from the payload.

**Variant not triggered.** `item/commandExecution/requestApproval` (the classic command approval) did
**not** appear: this build's code-mode `exec_command` tool fails inside the read-only sandbox and the
agent calls `request_permissions` instead. Its surface is
`thread-follower-command-approval-decision {conversationId, requestId, decision}` → `{ok:true}`
(`CODE`) — same family and envelope. **NOT_TRIGGERED**, not unsupported.

### POC-6 — Structured User Input: **NOT_TRIGGERED** (blocking path) / **PARTIAL**

Asked the agent to open a structured two-option question. It *did* invoke a user-input tool — the
**async** variant, `request_user_input_async` with arguments
`{"questions":[{"title":"Which probe option should I use?","options":["alpha","beta"]}]}`.

Outcome: an `AgentMessage` with `delivery:"async"` and embedded `questions:[{title,options}]` — **no**
`conversationState.requests[]` entry, **no** `item/tool/requestUserInput`. The async question is a
*message* answered by sending a reply (envelope, `CODE`: a `<send_user_message_question_reply>` JSON
block of `[{questionItemId, question, answer}]`).

The **blocking** path exists here but was unreachable in default collaboration mode:
`item/tool/requestUserInput` appears in `conversationState.requests[]` with params
`{threadId, turnId, itemId, questions:[{id,header,question,isOther,isSecret,options:[{label,description}]}],
isBlocking?, _meta?}`, and is answered with `thread-follower-submit-user-input`
`{conversationId, requestId, response:{answers:{<qid>:{answers:[…]}}}}` → `{ok:true}`.

Why it did not fire (`CODE`): `features.default_mode_request_user_input` defaults to **false** and is
explicitly forced off by a config transform (`tools.experimental_request_user_input.enabled:!1`). The
probe did **not** change the user's collaboration mode or permission policy, so plan mode is
**UNKNOWN**.

---

## 4. macOS IPC Protocol

**Transport** — Unix domain socket, `SOCK_STREAM`, one per user, served by the Desktop **main
process**. `~/.codex/ipc` 0700, socket 0600, uid-owned (`CODE`: `mkdirSync(dir,0o700)`,
`chmodSync(sock,0o600)`, `isSocket()`/owner-uid checks, group- or other-writable rejected). Recreated
each launch.

**Frame** — `uint32 LE length` + `UTF-8 JSON`, ceiling 268435456 B. **Byte-identical to Windows.**

**Envelope** — five types as on Windows: `request {requestId, sourceClientId?, version, method, params?,
targetClientId?, hostId?, timeoutMs?}`, `response {requestId, resultType, method?, handledByClientId?,
result?, error?}`, `broadcast {method, sourceClientId, targetClientIds?, params, version}`, plus the two
discovery frames (original envelope wrapped in `request`; answer `{canHandle}`).

**Fan-out** — a broadcast reaches every peer except the sender, filtered by `targetClientIds`
(`undefined` = all peers, `[]` = dropped unsent); the router rewrites `sourceClientId`. `hostId` ∈
`local | durable | remote-*`: clients put it in **params**; the version gate reads an envelope-level one.

**Version gate — the critical rule.** `version` is a per-method constant, not a counter (`CODE`, 26
entries): `thread-stream-state-changed` 11, `thread-follower-interrupt-turn` 4,
`thread-read-state-changed` 3, `thread-archived` 2, `start-turn` / `update-thread-settings` /
`edit-last-user-turn` / `thread-queued-followups-changed` 2, every other listed method 1, unlisted 0.
A broadcast must carry exactly the table value or is dropped silently; a request must too — except a
`thread-follower-*` carrying an explicit envelope `hostId`, which needs table+1. A mismatch fails
**discovery** first, so the caller sees `no-client-found`, not a version error.

| envelope `hostId` | version sent | expected | actual |
|---|---|---|---|
| *(none)* | 1 | 1 | **SUCCESS** |
| *(none)* | 0 | 1 | `no-client-found` |
| `local` | 1 | 2 | `no-client-found` |
| `local` | 2 | 2 | **SUCCESS** |

**Discovery** — `initialize` is intercepted by the router; other requests go to the client named by
`targetClientId`, else to all other clients in parallel (first claim wins). Candidates get
`client-discovery-request` and must answer `{canHandle}`; per-client timeout 10 s. Never answering
delays every `no-client-found` by 10 s for all peers (matches the public `openai/codex` report of
`SkyComputerUseService` causing 5 s timeouts).

**`requestId` is an integer** (`12`, `18`), not a UUID. **Thread state** (v11) carries snapshot/patches
with monotonic revisions, targeted at registered followers only; content is `conversationState`
(48 keys observed) plus canonical `turnHistory`; pending work is in `requests[]`.

**Control** — the Desktop client registers owner discovery, follower coordination and 18
`thread-follower-*` methods (`CODE`); `thread-follower-set-collaboration-mode` and
`-set-model-and-reasoning` are listed by third parties but **absent from this build**.

---

## 5. Windows vs macOS

| Capability | Windows (known) | macOS (measured) |
|---|---|---|
| IPC transport | Named Pipe | **Unix domain socket, `SOCK_STREAM`** |
| Endpoint | `\\.\pipe\codex-ipc` | **`~/.codex/ipc/ipc.sock`** (dir 0700, sock 0600) |
| Frame / max | 4-byte LE + JSON | identical / 268435456 B |
| `initialize` | supported | PASS — `clientId` = per-connection UUID |
| Official `clientType` | `desktop` | literal **not present** in this build |
| Owner discovery | supported | PASS — `{hostId,conversationId}` → `handledByClientId`; `hostId` must be `local` |
| Thread following | supported | **required precondition** (0 vs 1 frames) |
| Stream state | supported | PASS — v11; content in `turnHistory`, not `turns` |
| Start turn | supported | PASS — turn id matched persistence |
| Command approval | supported | observed via `item/permissions/requestApproval`; commandExecution variant NOT_TRIGGERED |
| `availableDecisions` | observed `[cancel]` | field **absent**; fixed enum incl. `"decline"` |
| `request_user_input` | Plan mode | NOT_TRIGGERED in default mode; async variant used instead |
| `requestId` type | string / number | **integer** (`12`, `18`) |
| Owner lifecycle | volatile | volatile; `client-status-changed`; new UUID per launch |
| **Version gate** | hinted | **mandatory per-method table + `hostId+1` rule, verified** |

---

## 6. Architecture Recommendation

1. **Unified `CodexDesktopIpcTransport`? Yes.** Protocol, discovery handshake, owner model,
   thread-state model, follower control surface and approval/user-input semantics are identical across
   platforms; only endpoint resolution and socket-permission handling differ. macOS needs an independent
   **Transport Backend**, not an independent protocol or control layer.
2. **Fully shareable** — frame codec, envelope schemas, the version table and gate, `initialize`, the
   discovery respond obligation, owner discovery, following and follower coordination,
   `thread-stream-state-changed` (incl. `turnHistory`), `thread-follower-start-turn` and the follower
   family, approval/user-input decisions, `client-status-changed` / `ipc-connection-reset` handling.
3. **macOS-specific** — resolve `$CODEX_HOME/ipc/ipc.sock` (default `~/.codex`) with the tmpdir legacy
   fallback; expect 0700 dir / 0600 socket, same-uid ownership, and a socket absent until the app
   launches and recreated each launch; never hard-code `/tmp/codex-ipc/ipc-<uid>.sock`.
4. **For the Tuvis spec:** (a) the version table, the `hostId+1` rule, and that mismatches surface as
   `no-client-found`; (b) `requestId` is an integer; (c) `availableDecisions` does not exist — use the
   fixed decision vocabulary; (d) turn content arrives via `turnHistory`, not `turns`; (e) `following`
   is a mandatory precondition; (f) always answer `client-discovery-request` with `canHandle:false`;
   (g) owner identity is per-connection and must be re-discovered after any Desktop restart.
5. **Can coding start now? Yes, with conditions.** Connect/initialize, owner discovery, follow + stream
   state, start turn and remote approval decisions are demonstrated end-to-end on macOS — enough for a
   transport-agnostic plan; two items can close during implementation (the commandExecution approval
   variant, the blocking `request_user_input` path).

---

## 7. Known Unknowns

1. Owner behaviour across a Desktop **restart** — not performed (would have disrupted live work);
   inferred to yield a new owner clientId.
2. Several threads sharing one `ownerClientId` — only one thread existed in this profile.
3. Blocking `request_user_input` in **Plan** mode — not tested; the user's mode was left untouched.
4. `item/commandExecution/requestApproval` params and decision acceptance here — not triggered.
5. The full patch `op` enum — only `remove` is compared literally in code.
6. Whether the socket is per app-instance or shared when two Desktop instances run.
7. Whether `thread-follower-set-collaboration-mode` / `-set-model-and-reasoning` exist elsewhere.
8. Whether the `version` table shifts between releases (a third-party table already differs here).

---

## 8. Final Verdict

**The macOS Codex Desktop exposes the same local IPC control plane as Windows — same framing, same
envelopes, same discovery, owner, thread-state and follower-control protocols — so Architecture A holds
cross-platform and macOS needs only its own Transport Backend; the one new hard requirement is that
every frame carry the correct per-method `version`.**

---

### Appendix — footprint and housekeeping

Only file left behind: this report. The harness, extracted bundles, raw envelopes and logs lived under
`/tmp/mac-ipc-probe/` and were deleted. Five turns were appended to
one of the user's Desktop threads during POC-4/5/6 — visible there, and the unavoidable cost of testing
injection against a genuinely Desktop-owned thread. No files were created outside `/tmp/mac-ipc-probe/`;
no system, network, SSH or permission settings changed; no token, cookie or API key was read.
