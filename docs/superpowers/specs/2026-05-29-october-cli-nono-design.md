# october CLI (run mode) with nono sandbox — design

**Date:** 2026-05-29
**Status:** Approved design, ready for implementation plan

## Goal

Add a single-process **CLI mode** to october (alongside the existing distributed
server mode) that:

- loads a workflow definition and a JSON config from files,
- runs the workflow against a given working directory,
- confines all untrusted tool execution with the [nono](https://github.com/always-further/nono)
  capability-based sandbox (Landlock on Linux, Seatbelt on macOS),
- supports suspend/resume when an agent pauses to ask the user a question.

The CLI is the **first end-to-end wiring of the full stack** in production code.
Today only `workflow/tests/workflow_e2e.rs` wires a `WorkflowActor`, and it uses a
`MockTransport` runtime client + `InMemoryJournal`.

## Non-goals

- No operator-facing WS `Server` (that stays for distributed mode).
- No SQLite / multi-run query surface (JSONL journal is enough for v1).
- No interactive TTY prompting for `ask_user` (suspend/resume instead).
- No nono-CLI wrapping or audit/snapshot/credential-proxy features (embed only).

## Threat model

The sandbox confines **untrusted, LLM-driven tool actions** (`bash`, file writes,
etc.), not a compromised october binary. The orchestrator process is *not*
sandboxed: it holds the API key and the LLM network connection by design. Only
the `october-runtime` child — which runs the tools — is sandboxed.

---

## Architecture

```
october process  (orchestrator — UNSANDBOXED: API key + LLM network)
 ├─ WorkflowActor / AgentActors  +  provider_registry (anthropic / mock)
 │     ├─ ExecutorClient( InMemExecutorTransport )      — runtime lifecycle
 │     └─ RuntimeClient( UnixSocketRuntimeTransport )   — tool calls (direct)
 │
 ├─ Executor  (REUSED: ProcessRuntimeProvider + RuntimeListenerServer
 │             + ConnectedRuntimeRegistry)
 │     └─ spawns october-runtime child ──[UNIX SOCKET]──▶ [NONO SANDBOX]
 │                                                          workdir RW + connect(socket) only
 └─ FileJournal  (NEW)
```

Two communication seams:

- **orchestrator ↔ executor** — in-memory channels (`InMemExecutorTransport`).
  Carries `ExecutorCommand` / `ExecutorEvent`. Used for runtime *lifecycle* only.
- **executor ↔ runtime child** — a **unix socket**, behind the `local` feature
  (off by default). The default executor↔runtime transport remains **TCP** and is
  untouched. Carries the runtime protocol (`RuntimeInboundMessage` /
  `RuntimeOutboundMessage`).

The orchestrator's **tool calls bypass the executor entirely**: with
`ProcessRuntimeProvider` the runtime is a local child, so the orchestrator talks
to it directly over the unix socket via `UnixSocketRuntimeTransport`. The
executor only does lifecycle (spawn / health / restart / destroy) and hands back
the transport — **its `do_tool_call` relay path is never invoked in CLI mode.**

Because of that, **the default (server/TCP relay) path is not modified at all.**
The unix-socket transport, the direct connection handler, and the in-process
executor transport are all additive and live behind the `local` feature.

**Transport mechanism and sandboxing are runtime spawn-arg decisions, not
provider traits.** `ProcessRuntimeProvider` is itself transport- and
sandbox-agnostic — it spawns the runtime binary with whatever endpoint and policy
it was *constructed* with:

- **CLI** constructs it with a **unix-socket endpoint + sandbox-on**.
- **Server mode / the e2e test** construct it with a **WebSocket(TCP) endpoint +
  sandbox-off** — unchanged from today.

The runtime selects its behavior purely from its arguments (see *Runtime
configuration* under the `runtime` crate). A future container / k8s / remote-VM
provider would bring its own listener + `RuntimeTransport` impl; everything
downstream only ever sees `Arc<dyn RuntimeTransport>`, so adding a provider does
not touch the registry, the `ExecutorClient`, or the CLI.

### Two clients, two transports

| Client | Purpose | Server-mode transport | CLI-mode transport |
|---|---|---|---|
| `ExecutorClient` | runtime lifecycle (create/destroy/query) | `WsExecutorTransport` | `InMemExecutorTransport` (new) |
| `RuntimeClient` | tool calls | `ExecutorWsTransport` (relay) | `UnixSocketRuntimeTransport` (direct, new) |

The orchestrator obtains a `RuntimeClient` uniformly in both modes:

```rust
executor_client.create_runtime(run_id, RuntimeConfig { working_dir }).await?;     // lifecycle
let rt: Arc<dyn RuntimeTransport> = executor_client.runtime_transport(run_id).await?;
let runtime_client = RuntimeClient::from_arc(rt);                                  // tools
```

What `runtime_transport` returns depends on the `ExecutorTransport` impl
(deep-module: the caller never knows whether bytes go direct or via relay):

| `ExecutorTransport` impl | `runtime_transport(id)` returns | Why |
|---|---|---|
| `InMemExecutorTransport` (CLI) | the live `UnixSocketRuntimeTransport` | executor in-process → hand back the socket-owning transport → bypass executor |
| `WsExecutorTransport` (server) | a relay `ExecutorWsTransport` bound to `id` | client remote → runtime socket lives on the executor → must relay |

### `RuntimeTransport` impls (consistent naming triad)

| impl | role |
|---|---|
| `UnixSocketRuntimeTransport` (new) | direct to a local runtime child over `WebSocketStream<UnixStream>` |
| `ExecutorWsTransport` (exists) | relay through the executor |
| `MockTransport` (exists) | tests |

---

## Components

### New crate: `executor-client`

Holds the lifecycle client and its transports, so the CLI does not depend on the
`server` crate (which carries the WS `Server`).

- `ExecutorTransport` trait — add `runtime_transport(runtime_id) -> Arc<dyn RuntimeTransport>`
  alongside the existing `send(request_id, cmd) -> mpsc::Receiver<ExecutorEvent>`.
- `ExecutorClient` (moved from `server`) — `create_runtime`, `destroy_runtime`,
  `query_runtimes`, and a delegating `runtime_transport`.
- `ClientError` (moved from `server`).
- `WsExecutorTransport` (moved from `server`) — its `runtime_transport` returns an
  `ExecutorWsTransport` relay bound to `runtime_id`.
- `server` is updated to import these from `executor-client` directly — **no
  re-export shim**.

Dependency direction: `executor-client → runtime-client` (for `RuntimeTransport`),
`executor-client → models`. The `InMemExecutorTransport` impl lives in the
`executor` crate (it needs executor internals); it implements
`executor-client::ExecutorTransport`.

### `executor` crate changes

**Default path is untouched.** The existing TCP `handle_runtime_connection`,
`do_tool_call` relay, and `ConnectedRuntimeRegistry` (storing a write-only
`RuntimeSink`) stay exactly as they are. Everything below is **behind the `local`
feature (off by default)**:

- **`UnixSocketRuntimeTransport`** — owns the accepted runtime link
  (`WebSocketStream<UnixStream>` split into sink + reader) and a
  `call_id → oneshot` pending map. Implements `RuntimeTransport`:
  - `invoke(call_id, call)` → register pending, send `RuntimeInboundMessage::ToolCall`,
    await `RuntimeOutboundMessage::ToolCallResponse` correlated by `call_id`.
  - `cancel(call_id)` → send `CancelCallRequest`.
  - A spawned reader task fills the pending map; supports concurrent in-flight calls.
- **Direct connection handler** — a `local`-only variant of the listener accept
  path: builds a `UnixSocketRuntimeTransport` from the accepted connection and
  registers it for retrieval. Unlike the relay handler, its read loop resolves the
  pending map rather than emitting `ExecutorEvent`s. The two handlers share only a
  tiny "read a `RuntimeOutboundMessage` frame" helper; the small duplication is a
  deliberate trade to keep the default relay path frozen.
- **`ConnectedRuntimeRegistry`** — gains `local`-gated transport storage:
  `register_transport(id, Arc<dyn RuntimeTransport>)` +
  `runtime_transport(id) -> Option<Arc<dyn RuntimeTransport>>`. Readiness
  signaling (`notify_when_ready`) is reused as-is. The default `sinks` map and
  relay methods are unchanged. Storage is `Arc<dyn RuntimeTransport>` so a future
  provider can register a different transport impl.
- **`RuntimeListenerServer`** — gains a unix-socket bind (`accept()` yields
  `WebSocketStream<UnixStream>`), used when the listener is configured for unix.

`ProcessRuntimeProvider` itself is **transport- and sandbox-agnostic** and is not
tied to `local`. It gains construction-time options for (a) the runtime endpoint
to pass (`--executor-url ws://…` vs `--executor-socket <path>`) and (b) an
optional sandbox policy (`--sandbox` + `--sandbox-read <path>` flags). The caller
chooses: CLI → unix + sandbox-on; server/e2e → WebSocket + sandbox-off (today's
behavior). **TODO (tracked):** when sandbox-on, spawn the child with
`Command::env_clear()` + a safe allowlist (`PATH`, `HOME`, locale) so the API key
cannot leak via `bash -c 'echo $ANTHROPIC_API_KEY'`.

`do_tool_call` is **not** modified — it is simply never reached in CLI mode.

### `actor` crate: `FileJournal`

Implements the existing opaque-bytes `Journal` trait. Constructed with a **root
dir** (from config, see *Config*); writes `<root>/runs/<persistence_id>/journal.jsonl`.

**Snapshots are no-op** (per design): runs are short, so we trade snapshotting for
always full-replaying the event log. Concretely:

- `persist(id, events)` — append each event to `journal.jsonl`, one record per
  line. Bytes are opaque, so each record is base64-encoded to keep the file
  strictly line-delimited regardless of content. Sequence numbers are implicit
  (1-based line position); the file is the only state.
- `replay(id, after_seq)` — stream records whose line index `> after_seq`, in
  order, base64-decoded back to `Vec<u8>`.
- `save_snapshot` / `copy_snapshot` / `delete_events_before` — **no-op**
  (`Ok(())`, nothing written). `latest_snapshot` — returns `Ok(None)`, so
  `spawn_root`'s recovery starts from `initial_state` and replays the full log
  (`after_seq = 0`) every time.
- `clear(id)` — remove the run's `journal.jsonl` (test helper).

Consequence: recovery is always a full replay from event 0. Acceptable for CLI run
lengths. (Fork — which relies on `copy_snapshot` — is not a CLI feature, so the
no-op is fine.)

### `runtime` crate (the sandboxed child)

**Runtime configuration — how the runtime knows its transport and whether to
sandbox.** Everything is driven by spawn args (set by `ProcessRuntimeProvider`):

- **Transport:** `--executor-url ws://<host:port>` → WebSocket/TCP; or
  `--executor-socket <path>` → unix socket. Exactly one is given; the runtime
  picks its transport accordingly.
- **Sandbox:** `--sandbox` requests confinement; absent → no nono (today's
  behavior). When `--sandbox` is given, capabilities are built from:
  - system paths (RO) — per-platform set required by the toolchain,
  - `--working-dir` (ReadWrite),
  - the executor unix socket path (`UnixSocketCapability`, connect only),
  - each `--sandbox-read <path>` (RO) — from the CLI config's `sandbox.extra_read_paths`.

Sequence in `main.rs`: parse args → if `--sandbox`, build the `CapabilitySet` and
call `Sandbox::apply()` **before** connecting or running any tool. **Fail-closed:**
if `Sandbox::support_info().is_supported` is false or `apply()` errors, exit
non-zero before connecting — there is **no bypass flag**; an unsupported platform
or a failed apply is a hard failure (the orchestrator surfaces it as a clear
sandbox error). Then connect on the chosen transport and run the existing tool
loop unchanged — `bash` and file tools need no change; confinement is inherited by
child processes.

The `--sandbox`/nono code path is behind the `runtime` crate's `sandbox` feature
(default on); a build without the feature has no `--sandbox` support and no `nono`
dependency, for unit tests and unsupported-platform development.

### New crate: `cli` (binary `october`)

- Arg parsing (`clap`), JSON config loading, provider registry construction,
  in-process executor assembly, event→terminal printing, the three subcommands.
- `TerminalSink: EventSink` — prints `AgentEvent`s (text chunks, tool start/complete,
  run complete) to stdout/stderr.

---

## Cargo features

CLI/local-only additions are gated behind crate features so a server/distributed
build compiles none of them (and avoids their deps), and the `cli` crate opts in
to exactly what it needs. Core types and the server path are always available.

| Crate | Feature | Gates | Default |
|---|---|---|---|
| `actor` | `file-journal` | `FileJournal` (filesystem-backed `Journal`) | off |
| `executor` | `local` | unix-socket listener bind, `UnixSocketRuntimeTransport`, the direct connection handler, registry transport storage/retrieval, `InMemExecutorTransport` | off |
| `executor-client` | `ws` | `WsExecutorTransport` (pulls `tokio-tungstenite`) | on |
| `runtime` (binary) | `sandbox` | `nono` dep + `Sandbox::apply` in `main` | on |

Notes:

- **The default executor↔runtime transport stays TCP and the server relay path is
  not modified.** Everything that the CLI's direct/bypass path needs is behind
  `executor/local`, off by default. A pure server build never compiles the unix
  socket, the direct handler, or the in-process transport.
- The `runtime_transport` accessor is added to the always-on
  `ExecutorTransport`/`ExecutorClient` interface (additive). `WsExecutorTransport`
  implements it by returning a relay transport; only the `InMemExecutorTransport`
  impl (which returns a *direct* `UnixSocketRuntimeTransport`) is behind
  `executor/local`.
- `runtime`'s `sandbox` feature is on by default (the shipped binary supports
  `--sandbox`); it can be compiled out for unit tests or unsupported-platform dev,
  in which case `--sandbox` is unavailable and there is no `nono` dependency. There
  is **no run-time bypass flag** — when `--sandbox` is requested, an unsupported
  platform or failed apply is a hard failure. The runtime accepts both
  `--executor-url` (TCP) and `--executor-socket` (unix); the CLI chooses unix.
- The `cli` crate enables: `actor/file-journal`, `executor/local`, and
  builds/locates the `october-runtime` binary with `sandbox` on. It can disable
  `executor-client/ws` for a pure-local build.

## Config (JSON)

`october.json`:

```json
{
  "providers": {
    "anthropic": {
      "type": "anthropic",
      "api_key_env": "ANTHROPIC_API_KEY",
      "base_url": "https://api.anthropic.com"
    }
  },
  "models": {
    "sonnet": {
      "provider": "anthropic",
      "model_id": "claude-sonnet-4-6",
      "max_tokens": 8192
    }
  },
  "sandbox": {
    "extra_read_paths": []
  },
  "storage": {
    "root_dir": "./.october"
  }
}
```

- `storage.root_dir` is the CLI's working root for run state: per-run data lives
  under `<root_dir>/runs/<run_id>/` (`journal.jsonl`, `manifest.json`). It is
  outside any agent `--workdir`. An optional `--state-dir` flag overrides it.
- A `WorkflowAgentDef.model` is a **model key** (e.g. `"sonnet"`) →
  `models.sonnet` → `providers.anthropic`.
- The CLI config is **CLI-owned policy**, hand-written serde in the `cli` crate —
  NOT a fluorite protocol type. The workflow file remains a pure
  `WorkflowDefinition` (fluorite JSON), reusable across server/CLI.
- `provider_registry: HashMap<String, Arc<dyn LlmProvider>>` is built keyed by
  model key: for each `models.<key>`, construct
  `AnthropicProvider::with_api_key(env).with_base_url(..).with_model(model_id)`
  (or a mock provider for `"type": "mock"`), inserted under `<key>`.

---

## Subcommands

### `october validate --workflow x.json --config october.json`

Structural + semantic checks; reports **all** errors; non-zero exit on any:

- `start` names an existing agent.
- every transition `to` names an existing agent.
- every transition `condition` parses as an `eval::Expr`.
- every `agent.model` ∈ config `models`.
- every referenced `model.provider` ∈ config `providers`.

### `october run --workflow x.json --config october.json --workdir DIR --input STR [--state-dir DIR]`

1. Run `validate`; abort on error.
2. Build `provider_registry`.
3. Start in-process `Executor` (`ProcessRuntimeProvider` configured for unix +
   sandbox-on, unix `RuntimeListenerServer`, shared `ConnectedRuntimeRegistry`).
4. `ExecutorClient(InMemExecutorTransport).create_runtime(run_id, RuntimeConfig { working_dir })`
   → spawns the nono-sandboxed child.
5. `executor_client.runtime_transport(run_id)` → `RuntimeClient::from_arc(..)`.
6. Build `WorkflowRuntimeContext { provider_registry, runtime_client, TerminalSink, DefaultToolboxFactory }`;
   `spawn_root(WorkflowActor::new(run_id, def, ctx), FileJournal(root_dir))`; send `Start { input }`.
7. Stream `AgentEvent`s to the terminal. On `AwaitingUserInput`: print the question
   + `run_id`, persist, `destroy_runtime`, exit with a distinct status. On
   finish/fail: `destroy_runtime`, exit with status.
8. Per-run state at `<root_dir>/runs/<run_id>/{journal.jsonl, manifest.json}`.
   `manifest.json` = resolved workflow def + workdir (**no secrets**).

### `october resume --run <run_id> --config october.json [--state-dir DIR] [--message STR]`

1. Load `manifest.json` (def + workdir) + `FileJournal` for `run_id`.
2. Re-create a fresh sandboxed runtime for the workdir (lifecycle wiring is not
   persisted — runtime context is rebuilt every run).
3. `spawn_root(WorkflowActor::new(run_id, def, ctx), FileJournal)` — recovery
   replays the journal to reconstruct workflow state.
4. Send `Resume { message }` and stream events as in `run`.

---

## Error handling

- **Sandbox unavailable / apply failure** → runtime child exits non-zero before
  connecting; `create_runtime` times out / fails; CLI reports a clear sandbox
  error and exits. Fail-closed; never runs tools unconfined — there is no bypass.
- **Validation failure** → all errors printed, non-zero exit, nothing spawned.
- **Provider/credential errors** (missing `api_key_env`) → fail during registry
  build, before spawning the runtime.
- **Runtime disconnect mid-run** → `UnixSocketRuntimeTransport` resolves pending
  calls with `TransportError::Disconnected`; the agent surfaces a tool error; the
  workflow's retry/failure model applies.
- **`Resume` on a non-awaiting workflow** → the `WorkflowActor` already owns this
  (no-op / error per its status machine).

---

## Security notes

- **Unix socket > TCP loopback under nono.** One `UnixSocketCapability` grants
  connect to exactly one socket path, versus allowing loopback networking broadly.
  The socket lives in the run dir (outside workdir), dir mode `0700`.
- **env scrub (TODO).** The runtime child must be spawned with a scrubbed
  environment so secrets in the orchestrator's env can't be read by a sandboxed
  `bash`. Tracked as a TODO on `ProcessRuntimeProvider`.
- **Linux Landlock caveat.** Path-based unix socket connect is governed by a
  filesystem rule on the socket path; we use a path-based socket (not abstract),
  so a filesystem rule covers it. Abstract-socket scoping would need newer
  Landlock — not used here.
- **macOS Seatbelt verification.** Confirm confinement holds for the real `bash`
  toolchain (compilers/git spawn helpers) during implementation.

---

## Crate / file layout

```
cli/                         NEW   binary `october`; subcommands, config, TerminalSink
                                   enables actor/file-journal + executor/local
executor-client/             NEW   ExecutorTransport trait (+ runtime_transport), ExecutorClient,
                                   ClientError, WsExecutorTransport [feat ws] (moved from server)
executor/                          DEFAULT PATH UNTOUCHED (TCP relay handler, do_tool_call, sinks).
                                   [feat local]: unix-socket listener, UnixSocketRuntimeTransport,
                                   direct connection handler, registry transport store/retrieve,
                                   InMemExecutorTransport, ProcessRuntimeProvider --executor-socket
                                   + env scrub TODO
runtime/                           [feat sandbox]: nono dep + Sandbox::apply in main + --sandbox
                                   /--sandbox-read flags, fail-closed (no bypass);
                                   always accepts --executor-socket and --executor-url
actor/                             + FileJournal [feat file-journal]; no-op snapshots
runtime-client/                    + RuntimeClient::from_arc; ExecutorWsTransport gains a
                                   runtime_transport-returns-relay constructor (additive)
server/                            imports ExecutorClient/etc from executor-client (no re-export)
```

Dependency direction (acyclic): `executor-client → runtime-client`;
`executor → executor-client + runtime-client`; `server → executor-client`;
`cli → cli-enabled executor + executor-client + runtime-client + workflow + actor + providers + models`.

---

## Testing

- **Unit**: `FileJournal` (persist/replay/snapshot/recover round-trips);
  `UnixSocketRuntimeTransport` invoke/cancel correlation (concurrent calls) over a
  paired in-memory or unix-socket harness; config parse + validation rules.
- **e2e** (`tests/`): full CLI `run` against a temp workdir with `mock-llm` driving
  a tiny two-agent workflow that calls `bash`; assert tool output and a final
  result. A suspend/resume e2e: an agent that `ask_user`s, `run` suspends,
  `resume` completes; assert journal replay reconstructs state.
- **Sandbox e2e** (platform-gated): assert a `bash` write outside `--workdir` is
  denied and inside is allowed.
- Workspace lints clean (`unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm`
  denied in production code).

---

## Open risks

- nono API surface for `UnixSocketCapability` / per-platform system paths must be
  validated against nono 0.59 during implementation (the embed example in nono's
  README uses `allow_path` + `block_network`; the unix-socket builder methods need
  confirming).
- The system-path allowlist for a real toolchain is the main ongoing maintenance
  surface; start minimal and expand from observed denials.
