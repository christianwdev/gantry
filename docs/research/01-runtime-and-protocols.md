# Gantry Architecture: Runtime, Agent Protocols & Windows Systems Layer

**Document:** `docs/research/01-runtime-and-protocols.md`  
**Owner:** Runtime, Protocol & Systems Architect  
**Status:** Architectural Blueprint (Pre-Implementation)  
**Target Environment:** Windows 11 primary (Rust `1.100.0`, Tauri v2, Node `24.18.1`, Bun `1.4.4`; clean cross-platform abstractions for macOS/Linux)

---

## Executive Summary & Core Decisions

Gantry is a local-first, single-user Tauri v2 desktop workbench for orchestrating **Claude Code**, **OpenAI Codex**, and **Google Antigravity**—including a first-party local MCP server that lets all three agents render live HTML mockups and real project React components directly inside Gantry's canvas.

Because our goal is to **solve every hard, foundational systems problem first** rather than ship a fragile prototype, this document establishes six non-negotiable architectural decisions:

1. **Native-Adapter Hybrid over ACP-Only:** Implement an internal Rust `AgentProvider` trait backed by **native wire adapters** for Claude Code (`stream-json` + control protocol) and Codex (`codex app-server` JSON-RPC 2.0), plus a first-class **ACP adapter** (`agent-client-protocol` crate) for Antigravity (`agy acp`)—giving us zero feature loss on Claude/Codex AND automatic extensibility for future ACP agents.
2. **One Local Streamable HTTP MCP Server for All Three Providers:** Host Gantry's Canvas & Workbench MCP tools directly inside the Rust Tauri backend using the official [`rmcp`](https://crates.io/crates/rmcp) crate (`StreamableHttpService`) on `http://127.0.0.1:<ephemeral-port>/mcp` with per-session 256-bit Bearer tokens that bind each MCP call to `(thread_id, worktree_path)` server-side. Inject it ephemerally into every spawned agent with **zero mutations** to `~/.claude.json`, `~/.codex/config.toml`, or `~/.gemini/antigravity-cli/settings.json`.
3. **Lossless Normalized Event Log + State Machine:** All three provider streams normalize into a single Rust `NormalizedEvent` enum driven by an explicit `SessionState` / `TurnState` finite state machine, persisted in an append-only **SQLite WAL** event store with materialized message snapshots for instant crash recovery and thread switching.
4. **Coalesced Tauri v2 `ipc::Channel` Streaming:** Never broadcast high-frequency token deltas over the global Tauri event bus. Stream per-thread ordered batches over `tauri::ipc::Channel<StreamBatch>` with 16 ms frame coalescing and sequence-cursor replay.
5. **Win32 Job Objects + `CREATE_NO_WINDOW` Everywhere:** Every spawned agent, MCP helper, git subprocess, and canvas dev server runs inside a Win32 Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (and breakaway disabled), spawned with `CREATE_NO_WINDOW` (`0x08000000`) and closed against the `CREATE_SUSPENDED` assignment race. Zero console flashes, zero orphaned processes on crash.
6. **Short-Path Worktrees & Lock-Aware Cleanup:** Isolate parallel threads in short-root git worktrees (mitigating Windows' 260-char `MAX_PATH` ceiling), strictly ban `node_modules` junctions/symlinks, and tear down worktree-scoped Job Objects before `git worktree remove`.

> **Verification Note (Codex 0.171.0 Local Schemas):** Direct filesystem reads against `docs/research/schemas/codex/` timed out awaiting out-of-workspace read approval in the subagent sandbox. All Codex App Server protocol methods below are verified against upstream `openai/codex` `codex-rs/app-server` documentation and `v2` protocol definitions; Section 2.2 includes a concrete **5-minute Schema Diff Checklist** to verify field-level exactness against the dumped `0.171.0` JSON schemas before writing `codex_adapter.rs`.

---

## 1. Architecture Decision: ACP-Only vs. Native-Adapter Hybrid

### 1.1 The Two Candidates

| Architecture | How Each Provider Connects | Dependencies Required on User Machine |
| :--- | :--- | :--- |
| **Option A: ACP-Only** | Rust `agent-client-protocol` client talks ACP JSON-RPC 2.0 to:<br>• `@agentclientprotocol/claude-agent-acp` (Node bridge wrapping Claude Agent SDK)<br>• `agentclientprotocol/codex-acp` (Rust bridge wrapping Codex)<br>• `agy acp` (native Google CLI) | `claude` + Node/npm `@agentclientprotocol/claude-agent-acp` + `codex-acp` binary + `agy` CLI |
| **Option B: Native-Adapter Hybrid (Recommended)** | Rust `AgentProvider` trait with 3 focused adapters:<br>• `ClaudeStreamAdapter` → `claude.exe` (`stream-json` + control protocol)<br>• `CodexAppServerAdapter` → `codex.exe app-server` (JSON-RPC 2.0)<br>• `AcpAdapter` → `agy.exe acp` (via official Rust `agent-client-protocol` SDK) | Only the 3 official CLIs the user already installs (`claude`, `codex`, `agy`). Zero third-party bridge binaries. |

### 1.2 Feature Parity & Operational Comparison

| Capability | ACP-Only (`claude-agent-acp` + `codex-acp` + `agy acp`) | Native-Adapter Hybrid (Claude native + Codex native + Antigravity ACP) |
| :--- | :--- | :--- |
| **Reasoning Effort / Thought Level** | **Lossy / Fragile.** Relies on ACP `session/set_config_option` (`configOptions`). Adapters lag behind new effort tiers (e.g., Claude `xhigh`/`max`, Codex `xhigh` or model-encoded reasoning flags) and UI pickers frequently desync when adapter capability tables go stale. | **Full Native Fidelity.** Claude `--effort <low\|medium\|high\|xhigh\|max>`, Codex `turn/start` `effort` + `summary` params, Antigravity via ACP `configOptions`. |
| **Dynamic Model Switching** | **Partial.** `session/set_config_option` works when adapters keep model catalogs updated; `codex-acp` has had regressions mapping reasoning-suffixed model strings. | **Full Native Fidelity.** Claude `control_request(set_model)`, Codex live account catalog + per-turn/thread model override, Antigravity via ACP `configOptions`. |
| **Mid-Turn Steering & Queueing** | **Not Standardized in Core ACP.** Core ACP `session/prompt` is strictly turn-sequential; mid-turn steering relies on non-standard vendor extensions (`_session/steering`) with inconsistent support across adapters. | **First-Class.** Codex exposes native `turn/steer` with optimistic concurrency (`expectedTurnId`); Claude supports bidirectional stdin queuing / interrupt-and-resume; Gantry's Rust orchestrator provides uniform client-side queueing where a provider lacks native steering. |
| **Interrupts / Cancels** | **Good.** Standard `session/cancel` notification across all three. | **Exact & Immediate.** Claude `control_request(interrupt)`, Codex `turn/interrupt`, Antigravity `session/cancel`. |
| **Tool Approvals & Sandbox Policies** | **Coarse-Grained.** Flattened into generic `session/request_permission` options; Codex granular sandbox profiles & exec policy rules or Claude permission mode switching (`plan`, `acceptEdits`, `auto`, `dontAsk`) lose fidelity or require editing config files. | **Exact Granularity.** Claude `can_use_tool` + `set_permission_mode`; Codex `item/commandExecution/requestApproval` & `item/fileChange/requestApproval` (`accept` / `decline` / `cancel` + policy rules); Antigravity `session/request_permission`. |
| **Structured User Questions** | **Weak / Unstandardized.** No universal structured multi-choice question primitive in core ACP v1; Claude's `AskUserQuestion` and Codex's `item/tool/requestUserInput` often degrade to plain text or break. | **Rich Native Cards.** Intercept Claude `AskUserQuestion` via `can_use_tool` (return structured `answers` via `updatedInput`), handle Codex `item/tool/requestUserInput` natively, map cleanly to one Gantry interactive question UI. |
| **Session Resume & Fork** | **Uneven.** ACP `session/load` / `session/resume` depends on each adapter implementing history replay; session **forking** is not universally supported in adapters. | **Complete.** Claude `--resume <id> --fork-session`; Codex `thread/resume` & `thread/fork`; Antigravity ACP `session/load` / `session/resume` (+ Gantry transcript fallback). |
| **MCP Server Injection** | **Clean in Theory, Mixed in Practice.** ACP `session/new` passes `mcpServers`, requiring each adapter to translate HTTP MCP + headers accurately into the underlying CLI. | **Direct & Deterministic.** Every provider connects directly to Gantry's single local Streamable HTTP MCP server using its own first-party config flag (`--mcp-config`, `-c mcp_servers...`, ACP `mcpServers`). |
| **Subagent Visibility** | **Flattened.** Nested Claude `Agent`/`Task` subagents and Codex cross-thread calls often collapse into opaque tool calls in ACP bridges. | **Structured Hierarchy.** Claude emits parent `tool_use_id` / subagent linkage in `stream-json`; Codex emits structured item/thread events; Gantry renders collapsible nested subagent trees. |
| **Installation Friction (Windows)** | **High.** Gantry must either bundle or download/auto-update `@agentclientprotocol/claude-agent-acp` (Node runtime + npm packages) and `codex-acp` binaries alongside the user's CLIs, doubling failure modes on Windows PATH/antivirus. | **Minimal.** Spawns `claude.exe`, `codex.exe`, and `agy.exe` directly from the user's machine. Zero extra bridges to bundle, update, or debug. |
| **Upstream Protocol Drift** | **Double Lag Risk.** You depend on *both* upstream CLI changes *and* community/Zed bridge releases catching up (note `@zed-industries/claude-code-acp` already underwent a package rename/migration to `@agentclientprotocol/claude-agent-acp`). | **Single-Hop Control.** Codex ships machine-generated JSON schemas (`codex app-server generate-json-schema`); Claude's `stream-json` + control protocol is the foundation of the official Claude Agent SDK; Antigravity is native ACP. |

### 1.3 Defended Recommendation

**Adopt Option B: Native-Adapter Hybrid behind a clean Rust `AgentProvider` trait.**

Why this is the decisive engineering choice for Gantry:
1. **No lowest-common-denominator tax:** Claude Code and Codex are Chris's daily drivers and are already installed locally (`claude 2.1.330`, `codex 0.171.0`). Routing them through third-party ACP wrappers throws away Codex's `turn/steer`, `thread/fork`, `item/tool/requestUserInput`, and `DynamicToolSpec`, as well as Claude's `AskUserQuestion`, `--fork-session`, `--effort`, and live `set_permission_mode`/`set_model`—while adding an extra Node process (`claude-agent-acp`) per thread on Windows.
2. **You still get a real ACP implementation for free:** Because Antigravity speaks ACP natively via `agy acp`, building `AcpAdapter` on the official [`agent-client-protocol`](https://github.com/agentclientprotocol/rust-sdk) Rust SDK means Gantry automatically gains a clean, spec-compliant ACP client that can plug in any future ACP agent (Gemini CLI, Goose, OpenCode, etc.) with near-zero code.
3. **Small, well-bounded adapter surface:** Each wire adapter is ~400–700 lines of isolated Rust mapping provider frames ↔ `NormalizedEvent`. All complexity—SQLite event sourcing, state machines, Win32 Job Objects, worktrees, MCP hosting, and Tauri `Channel` streaming—is written **once** in provider-agnostic core modules.

---

## 2. Exact Wire Protocols & MCP Injection Matrix

### 2.1 Provider 1: Claude Code (`claude.exe` Native `stream-json` + Control Protocol)

**Primary Sources:**
- Claude Code Headless / Programmatic Mode: https://code.claude.com/docs/en/headless
- Claude Code CLI Reference: https://code.claude.com/docs/en/cli-reference
- Claude Code MCP Configuration: https://code.claude.com/docs/en/mcp
- Claude Code Permissions: https://code.claude.com/docs/en/permissions

#### Process Invocation
Gantry spawns a persistent per-active-thread `claude.exe` worker (kept warm across turns in the same thread so MCP connections and prompt caches stay hot, and torn down on idle timeout or thread close):

```text
claude.exe
  --print
  --output-format stream-json
  --input-format stream-json
  --verbose
  --include-partial-messages
  --permission-prompt-tool stdio
  --model <model_id>                       # e.g. claude-opus-4-6 / sonnet / opus
  --effort <low|medium|high|xhigh|max>     # optional, when supported by model
  --permission-mode <default|acceptEdits|plan|auto|dontAsk|bypassPermissions>
  --mcp-config <temp_dir>/gantry-mcp-<session_id>.json
  --append-system-prompt <gantry_canvas_instructions>
  [--resume <native_session_id> [--fork-session]]
  [--session-id <uuid_v4>]                 # for deterministic new session IDs
```

**Important Flag Details:**
- `--verbose` is **required** alongside `--output-format stream-json` in `--print` mode; without it, Claude Code rejects or suppresses detailed stream frames.
- `--include-partial-messages` enables token-level `stream_event` frames (Anthropic SSE-equivalent `content_block_delta` events for text, thinking, and streaming tool JSON) rather than waiting for whole `assistant` turns.
- `--permission-prompt-tool stdio` routes tool authorization callbacks over the same NDJSON stdio pipe via `control_request` / `control_response` instead of hanging on a TTY prompt.
- **Do NOT pass `--strict-mcp-config` by default:** Passing `--strict-mcp-config` disables the user's existing `~/.claude.json` and project `.mcp.json` servers. Without it, `--mcp-config <file>` **merges** Gantry's local MCP server alongside the user's configured MCP servers. (We can expose a per-project "Isolate MCPs" toggle that enables `--strict-mcp-config` if desired.)
- **Ephemeral `--mcp-config` File:** Written to `%LOCALAPPDATA%\gantry\runtime\mcp-<uuid>.json` with restrictive ACLs and deleted when the worker exits:
  ```json
  {
    "mcpServers": {
      "gantry": {
        "type": "http",
        "url": "http://127.0.0.1:54321/mcp",
        "headers": {
          "Authorization": "Bearer ${GANTRY_MCP_TOKEN}"
        }
      }
    }
  }
  ```
  `GANTRY_MCP_TOKEN` is injected solely into the child process environment block (never persisted to disk). Claude Code expands `${GANTRY_MCP_TOKEN}` at runtime.

#### Bidirectional NDJSON Wire Contract (stdin / stdout)

1. **Host → Claude (`stdin`):**
   - **Initialize control handshake** (immediately after spawn):
     ```json
     {"type":"control_request","request_id":"req_init_1","request":{"subtype":"initialize"}}
     ```
   - **Send user turn:**
     ```json
     {"type":"user","message":{"role":"user","content":[{"type":"text","text":"Build the pricing table component"}]},"parent_tool_use_id":null,"session_id":"<id>"}
     ```
     *(Images attach as Anthropic `image` content blocks with `base64` source.)*
   - **Mid-session control requests:**
     - Interrupt active turn without killing process: `{"type":"control_request","request_id":"req_2","request":{"subtype":"interrupt"}}`
     - Live model switch: `{"type":"control_request","request_id":"req_3","request":{"subtype":"set_model","model":"claude-sonnet-4-6"}}`
     - Live permission mode switch: `{"type":"control_request","request_id":"req_4","request":{"subtype":"set_permission_mode","mode":"plan"}}`
   - **Responding to tool approval / `AskUserQuestion` (`control_response`):**
     ```json
     {
       "type": "control_response",
       "response": {
         "subtype": "success",
         "request_id": "req_from_claude_99",
         "response": {
           "behavior": "allow",
           "updatedInput": { ... }
         }
       }
     }
     ```
     Or denial: `{"behavior":"deny","message":"User rejected command","interrupt":false}`.
     - **Footgun — `AskUserQuestion`:** In Claude Code headless mode, `AskUserQuestion` arrives as a `can_use_tool` control request with `tool_name: "AskUserQuestion"` and `input.questions`. Gantry intercepts it, renders a native interactive question card, and replies `behavior: "allow"` with `updatedInput: { ...originalInput, answers: { ... } }`!

2. **Claude → Host (`stdout`):**
   - `{"type":"system","subtype":"init","session_id":"...","cwd":"...","model":"...","tools":[...],"mcp_servers":[...]}` — authoritative session metadata & MCP connection health.
   - `{"type":"stream_event","event":{...},"parent_tool_use_id":...}` — raw Anthropic streaming deltas (`message_start`, `content_block_start` [`text` / `thinking` / `tool_use`], `content_block_delta` [`text_delta` / `thinking_delta` / `input_json_delta`], `content_block_stop`, `message_delta` [usage], `message_stop`).
   - `{"type":"assistant","message":{...},"parent_tool_use_id":...}` — committed assistant turn block (includes `parent_tool_use_id` when emitted by a subagent!).
   - `{"type":"user","message":{...},"parent_tool_use_id":...}` — committed tool results (`tool_result` blocks).
   - `{"type":"control_request","request_id":"...","request":{"subtype":"can_use_tool","tool_name":"...","input":{...},"permission_suggestions":[...]}}` — blocks tool execution until Gantry writes `control_response`.
   - `{"type":"result","subtype":"success"|"error_max_turns"|"error_during_execution", "duration_ms":..., "total_cost_usd":..., "usage":{...}, "session_id":"..."}` — definitive turn completion boundary.

---

### 2.2 Provider 2: OpenAI Codex (`codex.exe app-server` v2 JSON-RPC 2.0)

**Primary Sources:**
- OpenAI Codex Repository (`codex-rs/app-server` & `codex-rs/app-server-protocol`): https://github.com/openai/codex/tree/main/codex-rs/app-server
- OpenAI Codex App Server Docs: https://developers.openai.com/codex/app-server
- Codex MCP & Config Reference: https://github.com/openai/codex/blob/main/docs/config.md

#### Process Invocation & MCP Injection
Gantry spawns a dedicated `codex.exe app-server` per active thread (clean per-thread MCP token scoping and Job Object isolation):

```text
codex.exe
  -c mcp_servers.gantry.url="http://127.0.0.1:54321/mcp"
  -c mcp_servers.gantry.bearer_token_env_var="GANTRY_MCP_TOKEN"
  -c mcp_servers.gantry.startup_timeout_sec=15
  -c mcp_servers.gantry.tool_timeout_sec=120
  app-server
```
With environment variable `GANTRY_MCP_TOKEN=<256-bit-hex-token>` set on the child process.

**Why `-c mcp_servers.gantry.*` over editing `~/.codex/config.toml` or `DynamicToolSpec`?**
1. **Zero Disk Side Effects:** `-c` CLI overrides are strictly process-scoped; running `codex` in Chris's external terminal will never see stale Gantry ports.
2. **Identical Tool Surface:** Codex connects to the exact same `rmcp` Streamable HTTP endpoint as Claude and Antigravity, so Opus 5.5 implements every Canvas tool **once**.
3. **Note on `DynamicToolSpec` (`dynamicTools` + `item/tool/call`):** Codex App Server *also* supports registering client-hosted tools directly in `thread/start` via `dynamicTools` (experimental API) where Codex sends a JSON-RPC `item/tool/call` (`DynamicToolCallParams`) server-to-client request over stdio. Keep this in our back pocket as a **Plan B fallback** if any Codex version exhibits Streamable HTTP MCP flakiness on `127.0.0.1` (we can bridge `item/tool/call` directly into the exact same Rust `CanvasToolRegistry` with ~80 lines of code!).

#### Strict JSON-RPC 2.0 Lifecycle (`stdio`, newline-delimited)

1. **Mandatory Initialization Handshake (requests before this are rejected):**
   - Client → Server (`initialize` request):
     ```json
     {
       "jsonrpc": "2.0",
       "id": 1,
       "method": "initialize",
       "params": {
         "clientInfo": { "name": "gantry", "title": "Gantry Workbench", "version": "0.1.0" },
         "capabilities": { "experimentalApi": true }
       }
     }
     ```
   - Server responds with negotiated server info/capabilities.
   - Client → Server (`initialized` notification — no `id`):
     ```json
     {"jsonrpc":"2.0","method":"initialized"}
     ```

2. **Thread Lifecycle (Client → Server Requests):**
   - `thread/start`: `{ cwd, model?, approvalPolicy?, sandbox?, baseInstructions?, developerInstructions?, config?, ... }` → returns `{ thread: { id, ... } }`.
   - `thread/resume`: `{ threadId, ... }` → reattaches to persisted Codex rollout/history.
   - `thread/fork`: `{ threadId, ... }` → forks an existing thread into a new independent `threadId`!
   - `thread/archive` / `thread/list`: manage persisted Codex threads.
   - `mcpServerStatus/list`: inspect Gantry MCP connection & discovered tools on startup (instant health check!).

3. **Turn Execution, Mid-Turn Steering & Cancellation (Client → Server Requests):**
   - `turn/start`: `{ threadId, input: [{ type: "text", text: "..." }, ...], model?, effort?, summary?, approvalPolicy?, sandboxPolicy?, cwd? }` → returns `{ turn: { id, status } }`.
   - `turn/steer`: `{ threadId, expectedTurnId, input: [{ type: "text", text: "Actually use Tailwind v4 tokens" }] }` → injects mid-turn user guidance into the **active** turn. `expectedTurnId` prevents race conditions if the turn just completed; if Codex rejects with turn-mismatch, Gantry cleanly falls back to `turn/start`.
   - `turn/interrupt`: `{ threadId, turnId }` → gracefully cancels active generation and running sandbox commands.

4. **Streaming Execution Notifications (Server → Client, no `id`):**
   - `turn/started`, `turn/completed` (includes final status `completed` | `interrupted` | `failed` and error details).
   - `item/started`, `item/completed` — structured item envelopes (`userMessage`, `agentMessage`, `reasoning`, `commandExecution`, `fileChange`, `mcpToolCall`, `webSearch`, etc.).
   - High-frequency deltas:
     - `item/agentMessage/delta` (assistant markdown chunks)
     - `item/reasoning/summaryTextDelta` / `item/reasoning/textDelta` (thinking stream)
     - `item/commandExecution/outputDelta` (live stdout/stderr from shell commands)
     - `item/fileChange/outputDelta` (streaming patch output)
     - `thread/tokenUsage/updated` (live context/token accounting)

5. **Interactive Server-Initiated Requests (Server → Client, WITH `id` — must reply!):**
   - `item/commandExecution/requestApproval`: asks user to approve a shell command (includes parsed command, cwd, reason, optional proposed execpolicy rule). Reply: `{ decision: "accept" | "acceptForSession" | "decline" | "cancel" }`.
   - `item/fileChange/requestApproval`: asks user to approve proposed file edits/diffs. Reply: `{ decision: "accept" | "acceptForSession" | "decline" | "cancel" }`.
   - `item/tool/requestUserInput`: structured multi-question prompt from Codex (experimental API). Gantry displays the same interactive question card used for Claude's `AskUserQuestion` and returns the user's structured answers!
   - `item/tool/call` (`DynamicToolCallParams`): only fired if we opt into `dynamicTools`.

#### Mandatory Pre-Code Schema Diff Checklist (`codex 0.171.0`)
Before writing `codex_adapter.rs`, run a 5-minute verification against the dumped schemas in `docs/research/schemas/codex/`:
1. `ClientRequest.json` → verify exact casing/method names for `thread/start`, `thread/resume`, `thread/fork`, `turn/start`, `turn/steer`, `turn/interrupt`, `mcpServerStatus/list`, and model listing (`model/list`).
2. `ServerNotification.json` → verify exact delta method names (`item/agentMessage/delta`, reasoning deltas, command output deltas, token usage notifications).
3. `ServerRequest.json` → verify exact approval decision enum strings (`accept`, `acceptForSession`, `acceptWithExecpolicyAmendment`, `decline`, `cancel`) and `item/tool/requestUserInput` response shape.
4. `v2/ThreadStartParams.json` & `v2/TurnStartParams.json` → verify whether `mcpServers` can be passed directly inside `config` JSON overrides (even cleaner than CLI `-c` flags!) and exact `effort` enum values.
5. `DynamicToolCallParams.json` → verify response schema in case we ever enable the `dynamicTools` fallback.

---

### 2.3 Provider 3: Google Antigravity (`agy.exe acp` via Official Rust ACP SDK)

**Primary Sources:**
- Agent Client Protocol (ACP) Specification: https://agentclientprotocol.com/
- ACP Rust SDK (`agent-client-protocol` / `agent-client-protocol-schema` / `agent-client-protocol-tokio`): https://github.com/agentclientprotocol/rust-sdk and https://docs.rs/agent-client-protocol
- Google Antigravity ACP Guide: https://antigravity.google/docs/cli/acp
- Google Antigravity IDE Extensions & ACP: https://antigravity.google/docs/ide/extensions/acp
- Google Antigravity CLI Reference: https://antigravity.google/docs/cli/reference
- Google Antigravity MCP Docs: https://antigravity.google/docs/mcp
- Google Antigravity Permissions: https://antigravity.google/docs/permissions

#### Process Invocation
```text
agy.exe acp --process-per-session --disable-telemetry
```

**Why These Exact Flags Are Critical:**
- **`--process-per-session`:** **Mandatory** when passing HTTP MCP servers to `agy acp` (per official Antigravity ACP docs). Without it, sessions share a worker process and HTTP MCP server bindings collide or fail to attach cleanly.
- **`--disable-telemetry`:** Disables telemetry reporting for the ACP server instance, honoring Gantry's strict local-only / no-telemetry constraint.
- **Note on `--ide-mcp-port`:** `agy acp` also documents an `--ide-mcp-port` flag for editor MCPs, **but** passing Gantry's server via standard ACP `session/new` `mcpServers` (with `type: "http"` and `Authorization: Bearer <token>` headers) is preferred because it is spec-standard ACP, carries our per-session bearer token, and works identically for any future ACP agent. Verify during initial `agy` integration test whether `agy acp` advertises `mcpCapabilities.http: true` in `initialize`; if it requires stdio MCP instead, our `gantry.exe --mcp-stdio-bridge` shim (Section 4.4) plugs straight into ACP `mcpServers` `stdio` transport with zero canvas changes!

#### Official ACP JSON-RPC 2.0 Lifecycle (via `agent-client-protocol` crate)
1. **`initialize` (Client → Agent):** Negotiate protocol version + client capabilities (`fs`, `terminal` — note: Gantry can initially let `agy` use its own built-in tools by advertising minimal client fs/terminal capabilities, or optionally expose Gantry-managed terminal/fs handlers later for unified diff previews). Inspect Agent's returned `agentCapabilities` (`loadSession`, `mcpCapabilities.http`, `promptCapabilities.image`).
2. **`authenticate` (Conditional):** Only if the agent returns `authMethods` / requires login; normally `agy` reuses its local system keyring OAuth credentials automatically.
3. **`session/new` (Client → Agent):**
   ```json
   {
     "cwd": "C:\\wt\\gantry\\a1b2c3d4",
     "mcpServers": [
       {
         "type": "http",
         "name": "gantry",
         "url": "http://127.0.0.1:54321/mcp",
         "headers": [
           { "name": "Authorization", "value": "Bearer <per-session-token>" }
         ]
       }
     ]
   }
   ```
   Returns `{ sessionId, modes?, configOptions? }`.
4. **Configuration Updates:** Use `session/set_config_option` for model selection and thought/effort level (and `session/set_mode` for mode switches when advertised in `session/new`).
5. **Resume:** Use `session/load` (or `session/resume` depending on negotiated capability) with saved `native_session_id` + `cwd` + fresh `mcpServers` (with Gantry's current ephemeral port & token!).
6. **Turn Loop (`session/prompt`):**
   - Client sends `session/prompt` with `sessionId` and content blocks (`text`, `image`, `resource_link`).
   - Agent streams `session/update` notifications:
     - `agent_message_chunk` → `NormalizedEvent::TextDelta`
     - `agent_thought_chunk` → `NormalizedEvent::ThinkingDelta`
     - `tool_call` / `tool_call_update` → `NormalizedEvent::ToolStarted` / `ToolDelta` / `ToolCompleted`
     - `plan` / `plan_update` → `NormalizedEvent::PlanUpdated`
     - `current_mode_update` → `NormalizedEvent::ModeChanged`
     - `usage_update` → `NormalizedEvent::UsageUpdated`
   - Agent calls `session/request_permission` → Gantry maps options (`allow_once`, `allow_always`, `reject_once`, `reject_always`) to the unified approval card and replies with the chosen `optionId`.
   - Client sends `session/cancel` notification to interrupt mid-turn; Agent unwinds pending tool calls/permissions and resolves `session/prompt` with `stopReason: "cancelled"`.

#### Honest Setup, Auth & Google ToS Notes for Antigravity
1. **Installation (currently missing from PATH):** Chris uses Antigravity inside T3 Code right now, which manages its own private Antigravity binary under `C:\Users\Chris\.t3\userdata\providers\antigravity\...`. **Gantry MUST NOT attempt to locate, invoke, or reuse T3 Code's private internal binary or auth tokens.** Instead, Chris should install the official standalone Antigravity CLI on Windows:
   ```powershell
   irm https://antigravity.google/cli/install.ps1 | iex
   agy --version
   agy acp --help
   agy            # One-time interactive Google OAuth sign-in into Windows Credential Manager / keyring
   ```
2. **Auth & ToS Compliance:** Invoking the user's own officially installed `agy acp` binary—authenticated via Google's official OAuth flow and governed by the user's own Google / Gemini Enterprise account quotas—is the exact sanctioned integration path documented at `antigravity.google/docs/cli/acp` (identical to Zed, JetBrains, and Xcode). Gantry should never spoof client credentials, bypass rate limits, or run unattended abuse loops.
3. **Graceful Unavailability:** Gantry's Provider Health Check on startup probes `claude --version`, `codex --version`, and `agy --version`. If `agy` is not installed yet, Claude and Codex work immediately while Antigravity shows a crisp "Install `agy` CLI" setup guide with the one-line PowerShell command and a "Re-check" button.

---

### 2.4 Consolidated MCP Injection Matrix

| Dimension | Claude Code (`claude.exe`) | OpenAI Codex (`codex.exe app-server`) | Google Antigravity (`agy.exe acp`) |
| :--- | :--- | :--- | :--- |
| **Transport to Gantry MCP** | Streamable HTTP (`type: "http"`) | Streamable HTTP (`url = ...`) | Streamable HTTP (`type: "http"` in ACP `mcpServers`; fallback: `gantry --mcp-stdio-bridge`) |
| **Injection Mechanism** | Ephemeral `--mcp-config <temp.json>` per session | Process-scoped `-c mcp_servers.gantry.*` CLI overrides | Per-session `mcpServers` array in `session/new` & `session/load` |
| **Auth Secret Passing** | `"Authorization": "Bearer ${GANTRY_MCP_TOKEN}"` (env var) | `bearer_token_env_var = "GANTRY_MCP_TOKEN"` (env var) | Header in stdin JSON-RPC `session/new` (in-memory pipe only, never written to disk) |
| **Mutates User Global Config?** | **No** (temp file deleted on exit) | **No** (CLI flags only) | **No** (in-memory RPC payload) |
| **Preserves User's Own MCPs?** | **Yes** (omit `--strict-mcp-config`) | **Yes** (`-c` merges into `~/.codex/config.toml`) | **Yes** (appends to `~/.gemini/antigravity-cli/settings.json`) |
| **Session Isolation Requirement** | 1 worker process per active thread | 1 app-server process per active thread | `--process-per-session` flag mandatory |
| **Health Verification** | `system(init).mcp_servers` status array | `mcpServerStatus/list` RPC after `initialized` | Tool discovery in first `session/new` + server-side `/mcp` initialize log |

---

## 3. Unified Internal Event & State-Machine Architecture (Rust)

### 3.1 Core Rust `AgentProvider` Trait & Normalized Event Model

Every provider adapter implements one async trait. Core orchestration, persistence, IPC streaming, and UI state never branch on provider wire formats.

```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    ClaudeCode,
    Codex,
    AntigravityAcp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLaunchConfig {
    pub thread_id: String,
    pub worktree_path: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub permission_policy: PermissionPolicy,
    pub resume_native_session_id: Option<String>,
    pub fork_from_native_session_id: Option<String>,
    pub system_prompt_appendix: Option<String>,
    pub mcp_endpoint: McpSessionEndpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSessionEndpoint {
    pub url: String,          // e.g. "http://127.0.0.1:54321/mcp"
    pub bearer_token: String, // 256-bit random token bound to (thread_id, worktree_path)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    Prompt,       // Ask on dangerous tools/writes
    AcceptEdits,  // Auto-approve file edits, prompt on shell
    PlanOnly,     // Read-only planning mode
    AutoApprove,  // Full autonomy inside worktree
}

/// Every wire event from Claude, Codex, or Antigravity normalizes into this enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedEvent {
    // --- Session Lifecycle ---
    SessionReady {
        native_session_id: String,
        model: Option<String>,
        available_models: Vec<ModelDescriptor>,
        mcp_servers: Vec<McpServerHealth>,
    },
    SessionConfigChanged {
        model: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
        permission_policy: Option<PermissionPolicy>,
    },
    SessionTerminated {
        exit_code: Option<i32>,
        reason: String,
    },

    // --- Turn Lifecycle ---
    TurnStarted {
        turn_id: String,
        user_message_id: String,
    },
    TurnCompleted {
        turn_id: String,
        status: TurnFinishStatus, // Completed | Cancelled | Failed | InterruptedByCrash
        usage: Option<TokenUsage>,
        cost_usd: Option<f64>,
        error: Option<String>,
    },

    // --- Streaming Content (coalesced before IPC) ---
    TextDelta {
        turn_id: String,
        item_id: String,
        parent_tool_id: Option<String>, // Non-null when emitted by a nested subagent!
        delta: String,
    },
    ThinkingDelta {
        turn_id: String,
        item_id: String,
        parent_tool_id: Option<String>,
        delta: String,
    },

    // --- Tool & Subagent Execution ---
    ToolStarted {
        turn_id: String,
        tool_call_id: String,
        parent_tool_id: Option<String>,
        tool_name: String,
        kind: ToolKind, // Shell | FileEdit | FileRead | Search | Mcp | Subagent | Other
        input: serde_json::Value,
    },
    ToolOutputDelta {
        turn_id: String,
        tool_call_id: String,
        stream: OutputStreamKind, // Stdout | Stderr | InputJson | Diff
        delta: String,
    },
    ToolCompleted {
        turn_id: String,
        tool_call_id: String,
        is_error: bool,
        output: serde_json::Value,
        diff_summary: Option<FileDiffSummary>,
    },

    // --- Planning / Tasks ---
    PlanUpdated {
        turn_id: String,
        steps: Vec<PlanStep>,
    },

    // --- Human-in-the-Loop Blocking Interactions ---
    ApprovalRequested {
        turn_id: String,
        interaction_id: String,
        tool_call_id: Option<String>,
        kind: ApprovalKind, // CommandExecution | FileChange | McpTool | Generic
        title: String,
        detail: serde_json::Value,
        options: Vec<ApprovalOption>,
    },
    UserInputRequested {
        turn_id: String,
        interaction_id: String,
        questions: Vec<StructuredQuestion>,
    },
    InteractionResolved {
        turn_id: String,
        interaction_id: String,
        resolution: serde_json::Value,
    },

    // --- Canvas Events (emitted directly by Gantry's local MCP Server!) ---
    CanvasArtifactPublished {
        turn_id: String,
        artifact_id: String,
        kind: String, // "html" | "react_component" | "diff_preview" (defined by Opus 5.5)
        payload: serde_json::Value,
    },

    // --- Usage & Diagnostics ---
    UsageUpdated {
        turn_id: String,
        usage: TokenUsage,
    },
    ProviderStderr {
        line: String,
    },
}

#[async_trait]
pub trait AgentSession: Send + Sync {
    fn provider(&self) -> ProviderKind;
    async fn send_prompt(&self, turn_id: &str, prompt: PromptInput) -> anyhow::Result<()>;
    async fn steer_turn(&self, active_turn_id: &str, prompt: PromptInput) -> anyhow::Result<SteerOutcome>;
    async fn cancel_turn(&self, active_turn_id: &str) -> anyhow::Result<()>;
    async fn respond_approval(&self, interaction_id: &str, decision: ApprovalDecision) -> anyhow::Result<()>;
    async fn respond_user_input(&self, interaction_id: &str, answers: serde_json::Value) -> anyhow::Result<()>;
    async fn update_config(&self, patch: SessionConfigPatch) -> anyhow::Result<()>;
    async fn shutdown(&self) -> anyhow::Result<()>;
}
```

### 3.2 Explicit `SessionState` & `TurnState` Finite State Machine

Never infer session state from scattered booleans (`isStreaming && !isWaitingApproval`). Enforce transitions in a single Rust actor per thread (`ThreadCoordinator`):

```text
SessionState:
  Dormant (no OS process; SQLite history only)
    ──[user sends prompt / warm-up]──► Starting (Job Object spawned; handshake in flight)
    ──[SessionReady]─────────────────► Idle (warm process; ready for turn)
    ──[send_prompt]──────────────────► ActiveTurn(TurnState)
    ──[shutdown / idle TTL / crash]──► Terminated ──► Dormant

TurnState (inside ActiveTurn):
  Running
    ├──[ApprovalRequested]──► AwaitingApproval { interaction_id } ──[respond_approval]──► Running
    ├──[UserInputRequested]─► AwaitingInput    { interaction_id } ──[respond_user_input]─► Running
    ├──[cancel_turn]────────► Cancelling (protocol cancel sent; 3s grace timer)
    └──[TurnCompleted]──────► (exits ActiveTurn → SessionState::Idle; pops next queued prompt if any)
```

**Mid-Turn Steering & Queueing Semantics (Unified Across All 3 Providers):**
- When the user submits a message while `SessionState::ActiveTurn(Running)`:
  - **Queue Mode (default):** Appended to `thread_queue` (persisted in SQLite so queued messages survive UI reloads); automatically dispatched when `TurnCompleted` fires.
  - **Steer Mode (explicit button / shortcut):**
    - On **Codex**: calls `turn/steer` with `expectedTurnId`. If `SteerOutcome::Applied`, no new turn is created; if `SteerOutcome::TurnAlreadyFinished`, immediately falls through to `turn/start`.
    - On **Claude Code / Antigravity**: cleanly surfaced in UI as either "Queue for next step" or "Interrupt & Redirect" (issues `interrupt`/`session/cancel`, waits for `TurnCompleted(Cancelled)`, immediately sends the steering message as the next turn). No silent drops, no lied-about capabilities.

### 3.3 High-Throughput Tauri v2 `ipc::Channel` Streaming Architecture

**Primary Sources:**
- Tauri v2 Calling the Frontend (Channels vs Events): https://v2.tauri.app/develop/calling-frontend/
- `tauri::ipc::Channel` Rust API Docs: https://docs.rs/tauri/latest/tauri/ipc/struct.Channel.html

**Why `Channel` Beats `app.emit()`:**
Tauri's global `app.emit()` broadcasts to all windows, performs no per-stream ordering guarantees across async tasks, and creates heavy WebView2 serialization overhead when multiple background threads stream simultaneously. `tauri::ipc::Channel<T>` is designed explicitly for ordered, high-frequency streaming scoped to one subscription.

**Concrete Gantry Streaming Protocol:**
1. **Per-Thread Subscription Command:**
   ```rust
   #[tauri::command]
   pub async fn subscribe_thread(
       state: tauri::State<'_, AppState>,
       thread_id: String,
       after_seq: i64,
       channel: tauri::ipc::Channel<StreamBatch>,
   ) -> Result<SubscriptionHandle, AppError>
   ```
2. **Sequence-Cursor Replay (Zero Missed Events on Tab Switch):**
   - Every persisted event in a thread gets a strictly monotonic `seq: i64` assigned by the `ThreadCoordinator` before SQLite commit.
   - When the React frontend mounts a thread view (or reconnects after HMR), it passes `after_seq` (the last sequence number already in its Zustand store).
   - Rust immediately replays `(after_seq..=current_seq]` from SQLite, then attaches the live broadcast receiver seamlessly with deduplication by `seq`.
3. **16 ms Frame Coalescing (WebView2 Anti-Jank Shield):**
   - Fast models (e.g., Sonnet/Gemini Flash/Codex) can emit 200–400 tiny token deltas per second, and verbose `cargo build` tool outputs can emit thousands of lines per second.
   - A lightweight per-subscription batcher in Rust buffers `TextDelta`, `ThinkingDelta`, and `ToolOutputDelta` frames, merging adjacent deltas with identical `(turn_id, item_id)` and flushing a `StreamBatch { events: Vec<SequencedEvent> }` every **16 ms** (~60 Hz) or when buffer size hits **16 KB**, while flushing **immediately** (0 ms delay) on lifecycle events (`TurnStarted`, `ApprovalRequested`, `UserInputRequested`, `CanvasArtifactPublished`, `TurnCompleted`).
   - Result: 10×–20× fewer WebView2 IPC JSON parses and React reconciliations with zero perceived latency.

### 3.4 SQLite WAL Persistence & Crash Recovery Schema

**Primary Sources:**
- SQLite Write-Ahead Logging (WAL): https://sqlite.org/wal.html
- SQLite Pragmas: https://sqlite.org/pragma.html

**Driver Recommendation: `rusqlite` (bundled SQLite 3) with a Dedicated Single-Writer Actor + Read Pool (`r2d2` / `deadpool`)**
- Why this beats naive async `sqlx` pools on Windows desktop: SQLite allows **one writer at a time**. Funneling all writes through a bounded `tokio::sync::mpsc` writer channel running on a dedicated OS thread with `rusqlite` eliminates `SQLITE_BUSY` write contention completely, lets us batch 20–50 streaming deltas into a single transaction every 50 ms (making SQLite sustain >100k events/sec!), while keeping concurrent UI reads lock-free via WAL snapshots.
- **Mandatory Connection Pragmas (applied on every connection open):**
  ```sql
  PRAGMA journal_mode = WAL;
  PRAGMA synchronous = NORMAL;       -- Safe against corruption in WAL mode; huge perf win
  PRAGMA busy_timeout = 5000;        -- 5s safety net
  PRAGMA foreign_keys = ON;
  PRAGMA temp_store = MEMORY;
  PRAGMA cache_size = -32000;        -- 32 MiB page cache
  ```

#### Core Schema (Event-Sourced + Materialized Snapshots)

```sql
CREATE TABLE projects (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    root_path       TEXT NOT NULL UNIQUE,
    default_branch  TEXT NOT NULL DEFAULT 'main',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE worktrees (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch_name     TEXT NOT NULL,
    path            TEXT NOT NULL UNIQUE,
    base_commit     TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('active', 'removing', 'removed', 'orphaned')),
    created_at      INTEGER NOT NULL
);

CREATE TABLE threads (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    worktree_id     TEXT REFERENCES worktrees(id) ON DELETE SET NULL,
    title           TEXT NOT NULL,
    provider        TEXT NOT NULL CHECK (provider IN ('claude_code', 'codex', 'antigravity_acp')),
    model           TEXT,
    reasoning_effort TEXT,
    permission_policy TEXT NOT NULL DEFAULT 'prompt',
    archived        INTEGER NOT NULL DEFAULT 0,
    last_seq        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE provider_sessions (
    id                TEXT PRIMARY KEY,
    thread_id         TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    provider          TEXT NOT NULL,
    native_session_id TEXT NOT NULL,
    parent_session_id TEXT REFERENCES provider_sessions(id),
    is_active         INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    UNIQUE(provider, native_session_id)
);

CREATE TABLE turns (
    id              TEXT PRIMARY KEY,
    thread_id       TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    session_id      TEXT REFERENCES provider_sessions(id),
    turn_index      INTEGER NOT NULL,
    status          TEXT NOT NULL CHECK (status IN (
                        'queued', 'running', 'awaiting_approval', 'awaiting_input',
                        'cancelling', 'completed', 'cancelled', 'failed', 'interrupted_by_crash'
                    )),
    prompt_json     TEXT NOT NULL,
    usage_json      TEXT,
    cost_usd        REAL,
    error_text      TEXT,
    started_at      INTEGER,
    completed_at    INTEGER
);

CREATE TABLE events (
    thread_id       TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    turn_id         TEXT REFERENCES turns(id) ON DELETE CASCADE,
    event_type      TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (thread_id, seq)
) WITHOUT ROWID;

CREATE TABLE message_snapshots (
    id              TEXT PRIMARY KEY,
    thread_id       TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    turn_id         TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    parent_tool_id  TEXT,
    role            TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool', 'system')),
    content_json    TEXT NOT NULL,
    last_seq        INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE canvas_artifacts (
    id              TEXT PRIMARY KEY,
    thread_id       TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    turn_id         TEXT REFERENCES turns(id) ON DELETE SET NULL,
    kind            TEXT NOT NULL, -- 'html' | 'react_component'
    title           TEXT NOT NULL,
    spec_json       TEXT NOT NULL,
    version         INTEGER NOT NULL DEFAULT 1,
    created_at      INTEGER NOT NULL
);

CREATE INDEX idx_turns_thread_status ON turns(thread_id, status);
CREATE INDEX idx_snapshots_thread_seq ON message_snapshots(thread_id, last_seq);
CREATE INDEX idx_canvas_thread ON canvas_artifacts(thread_id, created_at DESC);
```

**Crash Recovery Protocol (Runs Synchronously on App Boot):**
1. Because every agent subprocess lives inside a Win32 Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, if Gantry crashed, BSOD'd, or was killed in Task Manager, Windows **already terminated** all orphaned agents, shells, and dev servers automatically.
2. On startup, Gantry queries `SELECT id, thread_id FROM turns WHERE status IN ('running', 'awaiting_approval', 'awaiting_input', 'cancelling')`.
3. For each interrupted turn:
   - Fold any uncompacted `TextDelta` / `ThinkingDelta` / `ToolOutputDelta` rows from `events` into `message_snapshots` so **zero generated text is lost**.
   - Append a synthetic `NormalizedEvent::TurnCompleted { status: InterruptedByCrash }` to `events` and mark `turns.status = 'interrupted_by_crash'`.
4. When Chris opens that thread and hits **"Resume"**, Gantry spawns a fresh provider process using `provider_sessions.native_session_id` (`--resume` for Claude, `thread/resume` for Codex, `session/load` for Antigravity) and picks up right where the agent left off.

---

## 4. Windows-First Systems Footguns (Solve First!)

### 4.1 Process Tree Containment: Win32 Job Objects + `CREATE_NO_WINDOW`

**Primary Sources:**
- Microsoft Learn — Job Objects: https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
- Microsoft Learn — `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`: https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_extended_limit_information
- Microsoft Learn — Process Creation Flags (`CREATE_NO_WINDOW`): https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
- `process-wrap` Rust Crate (Tokio + Win32 JobObject): https://github.com/watchexec/process-wrap and https://docs.rs/process-wrap/latest/process_wrap/

**The 4 Windows Process Footguns & Exact Fixes:**
1. **Orphaned Grandchildren (Vite/Next.js dev servers, `cargo`, `esbuild`, PowerShell shells):**
   - `tokio::process::Child::kill()` on Windows calls `TerminateProcess` **only on the direct child PID**—every grandchild spawned by an agent leaks forever, holding TCP ports and locking files inside worktrees.
   - **Fix:** Create a dedicated Win32 Job Object per managed worker (`CreateJobObjectW`), set `BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` via `SetInformationJobObject(JobObjectExtendedLimitInformation)`, and **explicitly omit** `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` so no child can escape containment. Closing the Job Object handle (or Gantry crashing) atomically kills the entire descendant tree.
2. **The `CREATE_SUSPENDED` Assignment Race (and the `std::process::Command` Trap):**
   - If you spawn a child running normally and call `AssignProcessToJobObject` afterwards, a fast child (especially Node/Bun/PowerShell) can spawn a grandchild before assignment completes, escaping the job.
   - **Trap:** You **cannot** just pass `.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)` to `std::process::Command` because `std::process::Child` discards the primary `hThread` handle, leaving you unable to call documented `ResumeThread(hThread)` without resorting to undocumented `NtResumeProcess`!
   - **Fix:** Use [`process-wrap`](https://crates.io/crates/process-wrap) (`CommandWrap` with `CreationFlags(CREATE_NO_WINDOW)` + `JobObject`) — OR write a ~120-line Win32 spawner on `windows-sys` calling `CreateProcessW` with `CREATE_NO_WINDOW | CREATE_SUSPENDED`, calling `AssignProcessToJobObject(job, pi.hProcess)`, then `ResumeThread(pi.hThread)` and `CloseHandle(pi.hThread)`.
3. **Flashing Console Windows:**
   - Because Tauri builds with `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`, every `Command::new(...)` spawning a console binary (`claude.exe`, `codex.exe`, `agy.exe`, `git.exe`, `node.exe`) flashes a black `conhost.exe` window unless `CREATE_NO_WINDOW` (`0x08000000`) is set on **every single command invocation** in the entire codebase.
   - **Fix:** Ban raw `std::process::Command` and `tokio::process::Command` via a workspace Clippy lint (`disallowed_types` in `clippy.toml`!) and route 100% of spawns through `gantry_proc::ManagedCommand`.
4. **Cross-Platform Parity (macOS/Linux):**
   - On Unix (`#[cfg(unix)]`), `gantry_proc::ManagedCommand` sets `process_group(0)` (`setsid`/`setpgid`) and terminates via `kill(-pgid, SIGTERM)` → grace period → `SIGKILL` (plus `PR_SET_PDEATHSIG` on Linux).

### 4.2 Stdio Framing, Pipe Deadlocks, Large Payloads & Graceful Shutdown

1. **Windows Anonymous Pipe Deadlock Prevention:**
   - Windows anonymous pipes have a small default buffer (typically 4 KB). If Gantry writes a large prompt (e.g., 50 KB with pasted code or a base64 image) to `stdin` while the agent is simultaneously writing logs to `stderr` or events to `stdout`, synchronous/sequential I/O deadlocks permanently.
   - **Fix:** Always spawn **three independent Tokio tasks** per agent process:
     - `stdin_writer_task`: owns `ChildStdin` exclusively, pulls frames from a bounded `mpsc::channel(64)`, appends `\n`, flushes.
     - `stdout_reader_task`: owns `BufReader::with_capacity(64 * 1024, ChildStdout)` exclusively, reads raw bytes up to `b'\n'`.
     - `stderr_reader_task`: drains `ChildStderr` continuously into a ring buffer (last 500 lines) + emits `NormalizedEvent::ProviderStderr` so `stderr` can **never** fill and stall the child.
2. **UTF-8, CRLF & Multi-Megabyte JSON Lines:**
   - Never assume `BufRead::lines()` alone is safe: Windows CLIs occasionally emit `\r\n`, non-UTF-8 bytes in raw shell output, or **huge JSON lines** (>10 MB when a tool reads a large file or returns a base64 screenshot!).
   - **Fix:** Implement a custom `read_bounded_line(reader, max_bytes = 64 * 1024 * 1024)` using `read_until(b'\n', &mut buf)`: strip `\n` and `\r`, decode via `String::from_utf8` with `from_utf8_lossy` fallback, and cap at 64 MiB.
3. **Graceful Cancellation Ladder (Never Instant-Kill):**
   - Step 1 (`T = 0 ms`): Send protocol-native cancel (`interrupt` / `turn/interrupt` / `session/cancel`).
   - Step 2 (`T = 3,000 ms`, on shutdown or stuck cancel): Close `ChildStdin` (EOF).
   - Step 3 (`T = 5,000 ms`): Call `TerminateJobObject(job_handle, 1)` and drop the Job Object handle.

### 4.3 Git Worktree Orchestration on Windows

**Primary Sources:**
- Git Worktree Reference: https://git-scm.com/docs/git-worktree

**Windows-Specific Worktree Hardening:**
1. **Defeating `MAX_PATH` (260 chars):** Default worktree base directory to a **short root** (e.g., `C:\Users\Chris\.gantry\wt\<proj6>\<wt6>` or configurable `C:\wt\<wt6>`), and check `git config --get core.longpaths` + registry `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled` at startup.
2. **Never Symlink/Junction `node_modules`:** Let each worktree run its project setup hook (`bun install` / `pnpm install` / `npm ci`) which already uses global content-addressed hardlink stores safely on Windows.
3. **Lock-Aware Worktree Removal:** Before `git worktree remove --force <path>`, terminate the thread's Agent Job Object **and** Canvas Dev Server Job Object and await exit; retry with exponential backoff (200 ms, 500 ms, 1500 ms) for transient Defender/Indexer locks; if externally locked, mark `orphaned` and surface a clear recovery action (`git worktree unlock` + `git worktree prune`).
4. **Reliable Diff Generation:** Use `git status --porcelain=v1 -z -uall` and `git diff --no-ext-diff --no-color -U3` (with `core.quotepath=false`) via `gantry_proc::ManagedCommand`.

### 4.4 Local MCP Server Hosting in Rust (`rmcp` + `axum`)

**Primary Sources:**
- Model Context Protocol Specification (Streamable HTTP Transport): https://modelcontextprotocol.io/specification
- Official MCP Rust SDK (`rmcp`): https://github.com/modelcontextprotocol/rust-sdk and https://crates.io/crates/rmcp

**Architecture:**
Gantry embeds a single local MCP server inside the Tauri Rust backend, started once at app launch on `127.0.0.1:0` (ephemeral OS port, zero Windows Firewall prompt):
- `Axum` middleware rejects any request with a non-empty `Origin` header (anti-DNS-rebinding), validates `Authorization: Bearer <token>` in constant time (`subtle::ConstantTimeEq`), and attaches `McpCallerContext { thread_id, active_turn_id, worktree_path, project_root, provider }` from `McpTokenRegistry: DashMap<String, McpCallerContext>`.
- `rmcp::transport::streamable_http_server::tower::StreamableHttpService` handles `/mcp` JSON-RPC + SSE sessions and routes tool calls to Opus 5.5's `CanvasToolRouter`, which writes `canvas_artifacts` to SQLite and emits `NormalizedEvent::CanvasArtifactPublished`.
- Optional hidden subcommand `gantry.exe --mcp-stdio-bridge` proxies stdio ↔ `http://127.0.0.1:<port>/mcp` if any future agent lacks HTTP MCP support.

---

## 5. Interfaces with Opus 5.5 (Frontend & Canvas) & Open Questions

### 5.1 Explicit Contract with Opus 5.5 (Design Canvas & Frontend Architect)
1. **TypeScript Binding Generation:** Derive `specta::Type` (`tauri-specta`) or `ts_rs::TS` on all IPC commands, `StreamBatch`, `NormalizedEvent`, and `CanvasArtifact` structs to auto-generate `src/lib/bindings.ts`.
2. **Canvas MCP Tool Registration:** Runtime owns HTTP transport, auth, `McpCallerContext`, and `rmcp` `StreamableHttpService`. Opus 5.5 owns tool names, `schemars` input/output structs, and `CanvasArtifactPublished` payload schemas. Any bundler/dev-server subprocess spawned for live React component previews MUST use `gantry_proc::ManagedCommand` so it inherits the worktree's Win32 Job Object.
3. **Tauri IPC Command Surface:** `list_projects`, `register_project`, `list_worktrees`, `create_worktree`, `remove_worktree`, `list_threads`, `create_thread`, `fork_thread`, `archive_thread`, `get_thread_snapshot`, `subscribe_thread(thread_id, after_seq, Channel<StreamBatch>)`, `send_message`, `cancel_turn`, `respond_approval`, `respond_user_input`, `update_thread_settings`, `get_provider_health`, `get_worktree_diff`.
4. **Canvas Feedback Loop:** Align on whether interactive Canvas annotations flow back via `send_message` content blocks or a readable MCP resource (`canvas://current/annotations`).

### 5.2 High-Leverage Questions for Chris Before Writing Code
1. **Antigravity CLI (`agy`) Setup Timing:** Are you good installing the official standalone `agy` CLI (`irm https://antigravity.google/cli/install.ps1 | iex` + Google OAuth sign-in) so Gantry can talk to `agy acp` cleanly?
2. **Worktree Root Location on Windows:** Prefer a short path like `C:\Users\Chris\.gantry\wt\` (or `C:\gantry-wt\`), and should new threads default to a fresh worktree or let you choose *Main Checkout vs. New Worktree* per thread?
3. **Default Permission Posture:** `Prompt`, `AcceptEdits`, or `AutoApprove` inside isolated worktrees?
4. **External MCP Servers:** Keep loading your existing global/project MCP servers alongside Gantry's Canvas MCP (recommended default), or isolate sessions to Gantry-only MCP?
5. **Process Warm Pool Policy:** Is a configurable idle TTL (e.g., 15 min, max 4–6 warm workers with LRU eviction) the right balance of instant follow-ups vs. RAM?

---

## 6. Primary Source Reference Index

- **Agent Client Protocol (ACP) Specification:** https://agentclientprotocol.com/
- **ACP Rust SDK (`agent-client-protocol`):** https://github.com/agentclientprotocol/rust-sdk | https://docs.rs/agent-client-protocol | https://crates.io/crates/agent-client-protocol
- **ACP Adapters (Evaluated & Rejected for Claude/Codex):** https://github.com/agentclientprotocol/codex-acp | https://www.npmjs.com/package/@agentclientprotocol/claude-agent-acp
- **Claude Code Official Docs:** https://code.claude.com/docs/en/headless | https://code.claude.com/docs/en/cli-reference | https://code.claude.com/docs/en/mcp | https://code.claude.com/docs/en/permissions
- **OpenAI Codex App Server & Config:** https://developers.openai.com/codex/app-server | https://github.com/openai/codex/tree/main/codex-rs/app-server | https://github.com/openai/codex/blob/main/docs/config.md
- **Google Antigravity CLI, ACP & MCP Docs:** https://antigravity.google/docs/cli/acp | https://antigravity.google/docs/ide/extensions/acp | https://antigravity.google/docs/cli/reference | https://antigravity.google/docs/mcp | https://antigravity.google/docs/permissions
- **Model Context Protocol (MCP) & Official Rust SDK (`rmcp`):** https://modelcontextprotocol.io/specification | https://github.com/modelcontextprotocol/rust-sdk | https://crates.io/crates/rmcp
- **Tauri v2 IPC Channels:** https://v2.tauri.app/develop/calling-frontend/ | https://docs.rs/tauri/latest/tauri/ipc/struct.Channel.html
- **SQLite WAL & Pragmas:** https://sqlite.org/wal.html | https://sqlite.org/pragma.html
- **Win32 Job Objects & Process Creation Flags:** https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects | https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_extended_limit_information | https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
- **Rust Cross-Platform Process Wrapping (`process-wrap`):** https://github.com/watchexec/process-wrap | https://docs.rs/process-wrap/latest/process_wrap/
- **Git Worktree Documentation:** https://git-scm.com/docs/git-worktree
