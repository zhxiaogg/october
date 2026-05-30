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
  Carries `ExecutorCommand` / `ExecutorEvent`. Used for runtime *lifecycle*.
- **executor ↔ runtime child** — unix socket (was TCP). Carries the runtime
  protocol (`RuntimeInboundMessage` / `RuntimeOutboundMessage`).

The orchestrator's **tool calls bypass the executor**: the runtime is a local
child, so the orchestrator talks to it directly over the unix socket via
`UnixSocketRuntimeTransport`. The executor is responsible only for lifecycle
(spawn / health / restart / destroy).

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
| `UnixSocketRuntimeTransport` (new; was working name `RuntimeConnection`) | direct to a local runtime child over `WebSocketStream<UnixStream>` |
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
- `server` re-exports these for source compatibility.

Dependency direction: `executor-client → runtime-client` (for `RuntimeTransport`),
`executor-client → models`. The `InMemExecutorTransport` impl lives in the
`executor` crate (it needs executor internals); it implements
`executor-client::ExecutorTransport`.

### `executor` crate changes

- **`UnixSocketRuntimeTransport`** — owns the accepted runtime link
  (`WebSocketStream<UnixStream>` split into sink + reader) and a
  `call_id → oneshot` pending map. Implements `RuntimeTransport`:
  - `invoke(call_id, call)` → register pending, send `RuntimeInboundMessage::ToolCall`,
    await `RuntimeOutboundMessage::ToolCallResponse` correlated by `call_id`.
  - `cancel(call_id)` → send `CancelCallRequest`.
  - A spawned reader task fills the pending map; supports concurrent in-flight calls.
- **`ConnectedRuntimeRegistry`** — stores `Arc<UnixSocketRuntimeTransport>` per
  runtime (instead of today's write-only `RuntimeSink`). Adds
  `runtime_transport(runtime_id) -> Option<Arc<dyn RuntimeTransport>>`.
- **`handle_runtime_connection`** — builds a `UnixSocketRuntimeTransport` from the
  accepted connection and registers it (replaces inline response→`ExecutorEvent`
  translation).
- **`do_tool_call`** (server mode) — becomes `transport.invoke(call)` then wrap the
  result as `ExecutorEvent::ToolResult`. Net simplification: executor stops
  hand-rolling response correlation.
- **`RuntimeListenerServer`** — binds a `UnixListener`; `accept()` yields
  `WebSocketStream<UnixStream>`.
- **`ProcessRuntimeProvider`** — spawns the child with `--executor-socket <path>`
  (was `--executor-url ws://...`). **TODO (tracked):** spawn the child with
  `Command::env_clear()` + a safe allowlist (`PATH`, `HOME`, locale) so the API
  key cannot leak via `bash -c 'echo $ANTHROPIC_API_KEY'`.

### `actor` crate: `FileJournal`

Implements the existing opaque-bytes `Journal` trait:

- per `persistence_id`: an append-only `journal.jsonl` (one JSON-encoded event per
  line) + a `snapshot.json`, under the run directory (outside the workdir).
- `persist` appends; `replay(after_seq)` streams lines after a sequence;
  `save_snapshot` / `latest_snapshot` read/write the snapshot file;
  `delete_events_before`, `copy_snapshot`, `clear` as specified by the trait.
- Recovery uses `spawn_root`'s existing snapshot-plus-events replay.

### `runtime` crate (the sandboxed child)

- `october-runtime/src/main.rs`: after parsing `--working-dir` / `--executor-socket`,
  build a `nono::CapabilitySet` and call `Sandbox::apply()` **before** connecting
  or running any tool. **Fail-closed**: if `Sandbox::support_info().is_supported`
  is false or `apply()` errors, exit non-zero before connecting (the orchestrator
  surfaces this as a clear sandbox error). Capabilities:
  - system paths (RO) — per-platform set required by the toolchain,
  - `working_dir` (ReadWrite),
  - the executor unix socket path (`UnixSocketCapability`, connect only),
  - any `sandbox.extra_read_paths` from config, passed through as args.
- Connect to the unix socket and run the existing WS tool loop unchanged. `bash`
  and file tools need no change — confinement is inherited by child processes.
- `--dangerously-disable-sandbox` (off by default) skips `apply()` for debugging
  / unsupported platforms; prints a loud warning.

### New crate: `cli` (binary `october`)

- Arg parsing (`clap`), JSON config loading, provider registry construction,
  in-process executor assembly, event→terminal printing, the three subcommands.
- `TerminalSink: EventSink` — prints `AgentEvent`s (text chunks, tool start/complete,
  run complete) to stdout/stderr.

---

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
  }
}
```

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

### `october run --workflow x.json --config october.json --workdir DIR --input STR [--state-dir DIR] [--dangerously-disable-sandbox]`

1. Run `validate`; abort on error.
2. Build `provider_registry`.
3. Start in-process `Executor` (ProcessRuntimeProvider + unix `RuntimeListenerServer`
   + shared `ConnectedRuntimeRegistry`).
4. `ExecutorClient(InMemExecutorTransport).create_runtime(run_id, RuntimeConfig { working_dir })`
   → spawns the nono-sandboxed child.
5. `executor_client.runtime_transport(run_id)` → `RuntimeClient::from_arc(..)`.
6. Build `WorkflowRuntimeContext { provider_registry, runtime_client, TerminalSink, DefaultToolboxFactory }`;
   `spawn_root(WorkflowActor::new(run_id, def, ctx), FileJournal)`; send `Start { input }`.
7. Stream `AgentEvent`s to the terminal. On `AwaitingUserInput`: print the question
   + `run_id`, persist, `destroy_runtime`, exit with a distinct status. On
   finish/fail: `destroy_runtime`, exit with status.
8. Persist `./.october/runs/<run_id>/{journal.jsonl, snapshot.json, manifest.json}`.
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
  error and exits. Fail-closed; never run tools unconfined (unless
  `--dangerously-disable-sandbox`).
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
executor-client/             NEW   ExecutorTransport trait, ExecutorClient, ClientError,
                                   WsExecutorTransport (moved from server)
executor/                          + UnixSocketRuntimeTransport, InMemExecutorTransport,
                                   unix-socket listener, registry + do_tool_call refactor,
                                   ProcessRuntimeProvider --executor-socket + env scrub TODO
runtime/                           + nono Sandbox::apply in main, fail-closed, --executor-socket
actor/                             + FileJournal
runtime-client/                    + RuntimeClient::from_arc
server/                            re-export executor-client; do_tool_call via transport.invoke
```

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
