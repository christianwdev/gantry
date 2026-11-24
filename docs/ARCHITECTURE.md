# Gantry — Unified Architecture Specification

> **Status:** Locked v1 Blueprint (Pre-Code, Scope Decisions Confirmed) — 2026-11-24  
> **Authors:** Gemini 4 Neon (Lead & Runtime/Systems Architect) × Claude Opus 5.5 (Design Stage, MCP & Frontend Architect)  
> **Research Foundations:** [`docs/research/01-runtime-and-protocols.md`](research/01-runtime-and-protocols.md) · [`docs/research/02-design-canvas-and-frontend.md`](research/research/02-design-canvas-and-frontend.md)  
> **Target Environment:** Windows 11 primary (`rustc 1.100.0`, Tauri v2 + WebView2, `bun 1.4.4`, `node 24.18.1`, `claude 2.1.330`, `codex-cli 0.171.0`, `agy` CLI)

---

## 1. Mission, Non-Goals & Locked Scope Decisions

**Gantry** is a lean, local-first Tauri v2 desktop cockpit for messaging and orchestrating **Claude Code**, **OpenAI Codex**, and **Google Antigravity**—with a first-class **Design Stage** that lets every agent render live standalone mockups *and* real React components imported directly from your active workspace.

### Core Principle: Zero Ecosystem Lock-In
While Gantry is validated first against the owner's personal repos on Windows 11, **the architecture must never lock into a single package manager, framework, styling system, or agent provider**:
- **Package Manager & Runtime Agnostic:** Target workspace bootstrap (`install`), `postCreate`, dev server launch, and Promote `checks` auto-detect the workspace's package manager from `packageManager` / lockfiles (`bun.lock`/`bun.lockb` → `bun`, `pnpm-lock.yaml` → `pnpm`, `yarn.lock` → `yarn`, `package-lock.json` → `npm`, overridable in `.gantry/stage.json`). The Node Stage Runner runs on standard Node ≥20.19 (with Bun as fallback), never assuming Bun in user repos.
- **Provider Agnostic:** All UI, persistence, and Design Stage tooling speak only to the internal `AgentSession` trait + local `rmcp` MCP server. Adding a 4th agent (any ACP-compatible agent or custom CLI) requires zero frontend or canvas changes.
- **Framework & Styling Agnostic:** Tier 2 uses a pluggable `StageAdapter` registry (`vite-harness`, `next-vite`, `next-attach`, `url`, extensible to future frameworks) and always loads Vite, React, Sass, PostCSS, and Tailwind from the target project itself.

### Locked Scope Decisions (Confirmed 2026-11-24)

| Decision | Locked Choice |
| :--- | :--- |
| **Default workspace** | Main checkout by default; git worktree is an explicit per-thread toggle (short root `%USERPROFILE%\.gantry\wt\`, configurable). |
| **`.gantry/` config in repos** | Commit `.gantry/stage.json`, `.gantry/preview.tsx`, and `.gantry/mocks/` (like `.storybook/`); `.gantry/stage/` mockups always stay locally git-excluded via `$GIT_COMMON_DIR/info/exclude`. |
| **Fresh worktree bootstrap** | Auto-copy `.env*` + `.worktreeinclude` from main checkout, then run auto-detected package manager install (`bun`/`pnpm`/`yarn`/`npm`) + per-project `postCreate` hook. |
| **Next.js 16 `next-attach` (Phase 3)** | Attach to or own the single `next dev`, write hash-guarded git-excluded `app/(gantry)/gantry-stage/` routes, and allow agent-proposed dev-only `proxy.ts` exclusions with user approval. |
| **Default permission posture** | `Prompt` on Main Checkout, `AcceptEdits` inside Worktrees, and `stage_*` MCP tools auto-approved across all providers. |
| **External MCP servers** | Gantry MCP only by default (keeps RAM predictable and avoids unexpected elicitations), with a per-project opt-in toggle to load user global/project MCPs. |
| **Antigravity (`agy acp`) timing & skill** | Phase 2 delivery (prove Claude + Codex + Tier 1 first in Phase 1; run spike X4 early once `agy` is installed). Deliver instructions via MCP `instructions` + `gantry://guide` + `<gantry_context>` with zero global `~/.gemini` config mutations. |
| **First real Tier 2 targets** | Prove `vite-harness` on `vite-hono-base` + `hono-trpc-tanstack-base` first, with the `pockethost-panel` (`next-vite`) spike running in parallel. |
| **Resource Governor (32 GB host)** | ~40% soft RAM ceiling (~12.5 GB): max 4 warm idle workers (10 min TTL), 2 Stage Runners (10 min), 1 Gantry-owned `next dev` (20 min), 1 Edge browser w/ 4 tabs (5 min), 3 live iframes; never kills mid-turn. |
| **Mockup network / snapshots / pins / promote** | Mocked/offline by default (`network: "allow"` per-call opt-in); snapshots on `stage_snapshot`; pins attach 640 px JPEG crops; Promote is agent-led; 14-day retention under `%LOCALAPPDATA%\gantry\`. |

**Explicit Non-Goals:**
- No telemetry, analytics, cloud accounts, or login UI (reuses each local CLI's own authentication).
- No remote PC / cloud tunnel protocols.
- No bundled 200 MB Chromium binary on Windows (uses system Edge via CDP for headless verification).
- No throwaway MVP architecture: every hard foundational problem (process containment, wire protocols, worktree × runner lifecycle, WebView2 iframe isolation, event-sourced persistence, high-throughput streaming) is designed cleanly from Day 0.

---

## 2. High-Level System Architecture

```text
┌─────────────────────────────────── Gantry (Single-Instance Tauri v2 Process) ───────────────────────────────────┐
│                                                                                                                 │
│  ┌──────────────────────┐    ┌──────────────────────────┐    ┌───────────────────────────────────────────────┐  │
│  │  Agent Providers     │    │  Workspace & Process     │    │  Design Stage Subsystem                       │  │
│  │  • ClaudeStream      │◄──►│  Spine (gantry-proc)     │◄──►│  • StageService (single source of truth)      │  │
│  │  • CodexAppServer    │    │  • Workspace entity      │    │  • Tier1Compiler (crates/gantry-tier1-oxc)    │  │
│  │  • AntigravityAcp    │    │    (main | worktree)     │    │  • StageHost (axum 127.0.0.1:P2, ACAO:*)      │  │
│  │    (LocalSet actor)  │    │  • Win32 Job Objects     │    │  • RunnerSupervisor (1 Node runner per        │  │
│  └──────────┬───────────┘    │    (JOB_LIST +           │    │    (workspaceId, appRelPath) via stdio JSONL) │  │
│             │ ProviderEvent  │     HANDLE_LIST +        │    │  • NextAttach (.next/dev/lock owner/attach)   │  │
│             ▼                │     CREATE_NO_WINDOW)    │    │  • VerifyEngine (1 shared headless Edge in    │  │
│  ┌──────────────────────┐    │  • Resource Governor     │    │    app-scoped Job Object, CDP port=0)         │  │
│  │  ThreadCoordinator   │    │  • Real .exe + Reg PATH  │    │  • GitHygiene ($GIT_COMMON_DIR/info/exclude)  │  │
│  │  (Explicit FSM per   │    └──────────────────────────┘    └──────────────────────▲────────────────────────┘  │
│  │   active thread)     │                                                           │                           │
│  └──────────┬───────────┘    ┌──────────────────────────────────────────────────────┴────────────────────────┐  │
│             │ ThreadEvent    │  Local MCP Server (rmcp v3.x on axum 127.0.0.1:P1/mcp — separate port!)       │  │
│             ├───────────────►│  • Rejects non-empty Origin (anti-DNS-rebinding)                              │  │
│             │                │  • Constant-time Bearer token → (workspaceId, threadId, turnId, provider)     │  │
│             ▼                │  • 9 stage_* tools + gantry://guide resource + SSE progress notifications     │  │
│  ┌──────────────────────┐    └───────────────────────────────────────────────────────────────────────────────┘  │
│  │  SQLite WAL Writer   │                                                                                       │
│  │  (%LOCALAPPDATA%/    │  IPC Channels (tauri-specta v2 codegen, BigIntExportBehavior::Number):                │
│  │   gantry/gantry.db)  │  • thread_subscribe(threadId, sinceSeq, epoch, Channel<ThreadEventBatch>) ≤16ms batch │
│  │  Single writer actor │  • stage_subscribe(Channel<StageEvent>) — dock, runner logs, diagnostics, snapshots   │
│  │  + read pool         │  • status_subscribe(Channel<ThreadStatusEvent>) — sidebar dots                        │
│  └──────────────────────┘                                                                                       │
└─────────────────────────────────────────────────────────┬───────────────────────────────────────────────────────┘
                                                          ▼
         WebView2 (http://tauri.localhost, data-theme only, NO remote capabilities)
         ├── Left Rail: Projects → Workspaces/Threads + Stages list
         ├── Center: Virtualized Timeline (@tanstack/react-virtual + plain-TS ThreadLog + rAF) + Composer (Pins)
         └── Right Dock (Popout-capable): Live Stage <iframe> (P2 opaque / P3 or :3000 cross-origin) | Diff | Files
```

### Repository Workspace Layout

```text
gantry/
├── Cargo.toml                         # Rust workspace root
├── package.json                       # Bun workspace root (bun 1.4+, oxlint, oxfmt)
├── clippy.toml                        # Bans std::process::Command & tokio::process::Command
├── crates/
│   ├── gantry-proc/                   # Win32 Job Objects (JOB_LIST/HANDLE_LIST), CREATE_NO_WINDOW,
│   │                                  # real-.exe resolver, registry PATH, dunce::canonicalize
│   ├── gantry-protocol/               # Shared ThreadEvent, StageEvent, MCP schemas (specta + schemars)
│   ├── gantry-tier1-oxc/              # Pinned oxc_* crate (TS strip, JSX auto, data-gantry-loc pass)
│   ├── gantry-providers/              # AgentSession trait: Claude, Codex (exhaustive ServerRequest), ACP
│   ├── gantry-stage/                  # StageService, StageHost, RunnerSupervisor, VerifyEngine, NextAttach
│   ├── gantry-mcp/                    # rmcp v3.x Streamable HTTP server + Origin/Bearer tower middleware
│   └── gantry-db/                     # Single-writer rusqlite WAL actor + migrations + crash recovery
├── src-tauri/                         # Tauri v2 shell, single-instance guard, commands, tauri-specta export
├── packages/
│   ├── protocol/                      # Generated TS bindings (tauri-specta + Stage Runner JSONL types)
│   ├── stage-runner/                  # Node runner (vite-harness, next-vite via vite-plugin-storybook-nextjs)
│   ├── stage-runtime/                 # ≈8 KB injected iframe runtime (error/console/settle/inspector/pins)
│   └── agent-pack/                    # Canonical SKILL.md (gantry-design-stage) + framework wrapper recipes
└── apps/
    └── desktop/                       # React 19 + Vite 8 frontend (ThreadLog, Streamdown, Stage Dock, Diff)
```

---

## 3. Reconciled Architectural Decisions (All Cross-Review Conflicts Resolved)

### 3.1 Multi-Provider Runtime: Native-Adapter Hybrid + ACP
- **Decision:** Implement a Rust `AgentSession` trait with **native adapters** for **Claude Code** (`claude.exe --print --output-format stream-json --input-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio`) and **OpenAI Codex** (`codex.exe app-server` v2 JSON-RPC 2.0), plus a spec-compliant **ACP adapter** (`agent-client-protocol` Rust SDK) for **Google Antigravity** (`agy.exe acp --process-per-session --disable-telemetry`).
- **Why not ACP-only:** Third-party wrappers (`claude-agent-acp`, `codex-acp`) lose Codex's `turn/steer` (`expectedTurnId` + `clientUserMessageId`), `thread/fork`, `item/tool/requestUserInput`, and Claude's `AskUserQuestion`, `--fork-session`, `--effort`, and live `set_model`/`set_permission_mode`, while adding extra Node bridge processes and double upstream drift. Using `agent-client-protocol` for Antigravity still gives Gantry a reusable ACP client for any future agent.
- **Critical Protocol Hardening (from Round 2 Cross-Review):**
  1. **Codex Exhaustive `ServerRequest` Match:** Verified against local `codex-cli 0.171.0` schemas (`docs/research/schemas/codex/ServerRequest.json`), which define **10** server-initiated request methods. Every single method MUST be handled (UI card, auto-policy, or explicit JSON-RPC error—never silent drop):
     - `item/commandExecution/requestApproval` & `item/fileChange/requestApproval` (using externally tagged serde enum for `accept` / `acceptForSession` / `{acceptWithExecpolicyAmendment}` / `{applyNetworkPolicyAmendment}` / `decline` / `cancel`)
     - `item/tool/requestUserInput` (nested `{ answers: { [questionId]: { answers: string[] } } }`)
     - `mcpServer/elicitation/request` (auto-accept for Gantry's own tools; UI card for user MCPs)
     - `item/permissions/requestApproval`, `item/tool/call`, `account/chatgptAuthTokens/refresh`, `attestation/generate`, and legacy `applyPatchApproval` / `execCommandApproval`.
  2. **Codex Idempotent Steering:** Always pass Gantry's `clientUserMessageId` in `turn/steer` (with `expectedTurnId`) so a steer→`turn/start` fallback never duplicates a message.
  3. **Claude Control Protocol Completeness:** Handle inbound `control_cancel_request` (dismissing stale sticky approval cards immediately), return explicit errors for unknown `control_request` subtypes (`hook_callback`, `mcp_message`), and intercept both `AskUserQuestion` and `ExitPlanMode` through `can_use_tool`. Store both `question_id` and `question_text` in Gantry's unified question-card model so Claude (keyed by text) and Codex (keyed by ID) map cleanly.
  4. **Claude Launch Hardening:** Use `--append-system-prompt-file` (never multi-line inline argv), ephemeral `--mcp-config <file>` (never inline token in argv), `DISABLE_AUTOUPDATER=1` in worker env, and verify Git Bash (`CLAUDE_CODE_GIT_BASH_PATH`) during startup health checks.
  5. **ACP `?Send` / `LocalSet` Isolation:** Run each `agent-client-protocol` connection inside a dedicated single-threaded actor task communicating over `tokio::sync::mpsc`, exposing a clean `Send + Sync` `AgentSession` handle to `ThreadCoordinator`.
  6. **Antigravity Standalone CLI & Auth Compliance:** Never touch T3 Code's private internal Antigravity binary/auth. Gantry invokes the user's officially installed `agy` CLI (`irm https://antigravity.google/cli/install.ps1 | iex`).

### 3.2 First-Class `Workspace` Entity (Bridging Worktrees × Design Stage)
The biggest architectural seam uncovered in cross-review was Worktrees colliding with Tier 2 Stage Runners. The fix is a first-class **`Workspace`** entity (either `kind: "main"` or `kind: "worktree"`):
- **Owns:** canonical path (via `dunce::canonicalize` with lowercased drive letter — never `\\?\` verbatim paths), branch, `bootstrap_status` FSM (`created → files_copied → deps_installing → ready | failed`), a `JobGroup` (agent jobs, Stage Runner jobs, managed `next dev` job), `notify` file watchers, and a process-wide mutex-serialized `$GIT_COMMON_DIR/info/exclude` writer.
- **Bootstrap Pipeline for Fresh Worktrees:**
  1. Copy `.env*` + any paths listed in `.worktreeinclude` from the main checkout.
  2. Auto-detect the target workspace's package manager (`bun` / `pnpm` / `yarn` / `npm` via `packageManager` field or lockfile, overridable in `.gantry/stage.json`) and run its install command + any per-project `postCreate` codegen (e.g., `routeTree.gen.ts`, contracts) inside the worktree's Job Object.
  3. If an agent invokes `stage_render` while `bootstrap_status != ready`, Tier 2 returns `status: "preparing"` with live MCP progress notifications (never a cryptic Vite module-resolution crash).
- **Stage Runner Keying:** Keyed by **`(workspace_id, app_rel_path)`** (never `project_root`) so parallel worktrees of the same repo never render from the wrong branch.
- **Immutable Revisions for BOTH Tiers:** On every Tier 1 AND Tier 2 revision, snapshot the mockup files + PNG thumbnails into **`%LOCALAPPDATA%\gantry\stages\<stageId>\<rev>\`** (never Roaming `%APPDATA%` and never inside the worktree alone). Removing a worktree never breaks stage history, and the UI rev scrubber works cleanly even after HMR overwrites `.gantry/stage/*`.
- **Deterministic Teardown Order (Preventing Windows Deletion Locks):**
  1. Stop Gantry's `notify` (`ReadDirectoryChangesW`) directory watchers on the workspace.
  2. Close headless Edge tabs pointing at the workspace's runner origins.
  3. Terminate Stage Runner & Gantry-owned `next dev` Job Objects (never kill an *externally* started `next dev`).
  4. Terminate Agent Job Objects.
  5. Wait for every job's `ActiveProcesses == 0`.
  6. Run `git worktree remove --force` with exponential backoff (200 ms, 500 ms, 1500 ms) for transient Defender/Indexer locks.

### 3.3 Two-Layer Event Pipeline, Codegen & Single SQLite WAL Store
- **Two Event Layers:**
  - `ProviderEvent` (internal to `gantry-providers`, never persisted directly) → folded by `ThreadCoordinator` FSM into:
  - `ThreadEvent` (persisted in SQLite + streamed over `Channel<ThreadEventBatch>` + codegen'd to TypeScript).
  - Raw provider NDJSON frames also append to a rotating per-session diagnostic log on disk (invaluable for debugging drift and generating golden adapter test fixtures).
- **Unified `ThreadEventBatch`:** `{ thread_id, epoch, from_seq, to_seq, events }`:
  - `epoch` increments on backend restart, cleanly decoupling **≤16 ms / ≤64-event UI channel flushes** from **50 ms SQLite writer batch commits** without stale-cursor bugs.
  - Inline stage cards are first-class items (`item.kind = "stage"`, carrying `stage_id`, `rev`, `status`, `thumb_path`) — timeline renders static PNG thumbnails, never live iframes.
  - Pending interactions (`Approval`, `Question`) are items; on `TurnCompleted`, cancel, or crash, `ThreadCoordinator` automatically emits `InteractionResolved { cancelled }` for every open interaction in that turn.
- **Two Separate Low-Frequency Channels:** `stage_subscribe(Channel<StageEvent>)` for the Stage Dock/Runner/Diagnostics, and `status_subscribe(Channel<ThreadStatusEvent>)` for sidebar status dots.
- **Single `gantry.db` (SQLite WAL under `%LOCALAPPDATA%\gantry\`):**
  - One dedicated single-writer `rusqlite` OS thread (`journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`) + concurrent read pool.
  - Holds ALL tables atomically (`projects`, `workspaces`, `threads`, `provider_sessions`, `turns`, `thread_queue`, `events` — **with standard ROWID, NOT `WITHOUT ROWID`** due to large tool payloads, `message_snapshots` with `compacted_through_seq` watermark, plus `stages`, `stage_revisions`, `stage_diagnostics`, `stage_snapshots`, `stage_pins`, `compare_groups`).
  - Raw PNGs live on disk only; SQLite stores metadata & paths.
  - **`tauri-plugin-single-instance`** enforced at startup so two Gantry processes can never corrupt each other's crash-recovery sweep.
- **One Type Generator (`tauri-specta` v2 + `schemars`):**
  - `BigIntExportBehavior::Number` configured explicitly so `seq: u64` exports cleanly.
  - CI test regenerates `packages/protocol` bindings and fails on diff.

### 3.4 Two-Tier Design Stage & Local MCP Server
Tailored specifically to the owner's real portfolio (Next 16 + SCSS modules + Bun monorepo UI kits `@pockethost/ui` & `@airfield/ui`, Vite 8 + TanStack Start + React Router 8 + React Compiler, Tailwind v4 with `@media (prefers-color-scheme)`):

- **Tier 1 (Zero-Setup Standalone HTML / CSS / Tailwind v4 / TSX):**
  - Compiled in-process by Rust (`crates/gantry-tier1-oxc`) → served as native ESM + import map of **vendored, build-time bundled libraries** (zero CDN) + vendored `@tailwindcss/browser` from **Stage Host (`127.0.0.1:P2`, `ACAO: *`)** into a `sandbox="allow-scripts"` opaque-origin iframe (`connect-src 'none'` by default).
- **Tier 2 (Real Project Components — Hybrid 4 Adapters):**
  - `vite-harness` (default): spawns project's own `vite` + React (`resolve.dedupe: ['react', 'react-dom']`), strips SSR/codegen plugins (`tanstack-start*`, `@tanstack/router-plugin`, `react-router`, etc.), remaps `.gantry/` imports through a virtual app importer so multi-target aliases (`@components/*` → 2 dirs) and nested tsconfigs resolve natively, appends Tailwind v4 `@source` in memory, injects dev-only `data-gantry-loc` via Oxc AST pass (since React 19 removed `_debugSource`).
  - `next-vite`: wraps `vite-plugin-storybook-nextjs` (pinned) + maps `next.config` `sassOptions` → Vite `scss.loadPaths` + caches `next/font/google` offline under `%LOCALAPPDATA%\gantry\fonts-cache`.
  - `next-attach` (Phase 3): detects/attaches to or owns the single allowed Next 16 `next dev` (`.next/dev/lock`, verifying PID/port liveness to recover stale locks) and writes hash-guarded, git-excluded temporary routes under `app/(gantry)/gantry-stage/<id>/page.tsx` for full async RSC/root-layout fidelity.
  - `url`: frames any route of a running dev server (e.g., SolidStart) with CDP-only diagnostics.
- **Verification Engine (Headless System Edge via CDP):**
  - Runs 1 shared `msedge.exe` (`--headless=new --remote-debugging-port=0 --user-data-dir=%LOCALAPPDATA%\gantry\verify-edge-profile`) in an **app-scoped** Win32 Job Object (setting *only* `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so Edge's nested sandbox child jobs work cleanly on Windows 11).
  - Always uses `Emulation.setDeviceMetricsOverride` + `Emulation.setEmulatedMedia` (avoiding Edge's `--window-size` 518 px layout bug and mobile 980 px meta-viewport trap).
  - Returns structured build/runtime/console/network/layout diagnostics (`horizontalOverflow`, `clippedText`, `emptyRender`, `brokenImages`, `fontsNotLoaded`) with `isError: true` on fatal errors so agents self-heal *before* presenting to the user.
- **WebView2 Security & Theming:**
  - Startup check verifies Evergreen Runtime version via `wry::webview_version()` ≥ `(123, 0, 2420, 47)` (the runtime corresponding to WebView2 SDK `1.0.2365.46`), respecting `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`.
  - Never serve stages from Tauri custom protocols or `srcdoc`/`blob:`. Every stage runs on loopback HTTP (`P2` or `P3` or `:3000`) → `Origin::Remote` to Tauri with **zero** `remote.urls` capabilities.
  - Per-stage dark/light uses `iframe.style.colorScheme` (empirically verified in Edge 158) + wrapper `theme` prop; Gantry's own UI uses `data-theme` exclusively.
  - Strip `X-Frame-Options` / `frame-ancestors` on `localhost:*` document responses via WebView2 `WebResourceRequested` so project security headers don't break the dock iframe.
- **Local MCP Server (`rmcp` v3.x on `axum`, `127.0.0.1:P1/mcp`):**
  - Separate port from Stage Host `P2`.
  - Rejects non-empty `Origin` headers (403) + constant-time Bearer token validation.
  - Supports MCP `2025-11-25` (and `2026-07-28` via `rmcp` v3.x) with 9 tools: `stage_render`, `stage_update`, `stage_compare`, `stage_snapshot`, `stage_inspect`, `stage_feedback`, `stage_list`, `stage_setup`, `stage_close`, plus `gantry://guide` resource and SSE progress notifications.
  - Ephemeral MCP injection: Claude `--mcp-config <temp.json>` + `GANTRY_MCP_TOKEN`; Codex `-c mcp_servers.gantry.*` (`tool_timeout_sec=180`, `default_tools_approval_mode="auto"`, `bearer_token_env_var="GANTRY_MCP_TOKEN"`); Antigravity ACP `session/new.mcpServers` (`--process-per-session`).
  - Dual image return on `stage_snapshot`: inline downscaled PNG/JPEG `image` block + `resource_link` to disk path (guaranteed vision across Claude, Codex, and Antigravity).
  - Pin crops → composer attachments: Claude base64 `image` block, Codex `localImage` path, ACP capability-gated `image` / `resource_link`.

### 3.5 Windows Process Spine & Resource Governor
- **Mandatory `gantry_proc::ManagedCommand`:** Raw `std::process::Command` and `tokio::process::Command` are banned via `clippy.toml`. Every child process uses:
  - `CREATE_NO_WINDOW` (`0x08000000`)
  - Atomic `PROC_THREAD_ATTRIBUTE_JOB_LIST` (Windows 10+, cleaner than `CREATE_SUSPENDED` + `ResumeThread`) + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (prevents Job handle leakage into children!)
  - Direct `.exe` resolution (never `.cmd` shims, avoiding BatBadBut/TOML quote mangling) + registry `HKCU\Environment`/`HKLM` PATH merging for Start-menu launches.
  - 3 dedicated Tokio stdio tasks per agent (`stdin` writer, 64 MiB bounded `stdout` CRLF/lossy-UTF-8 reader, continuous `stderr` ring-buffer drainer).
- **Resource Governor (scaled via `GlobalMemoryStatusEx`, polled every 5 s):**
  - Never sets hard `JOB_OBJECT_LIMIT_JOB_MEMORY` caps (which cause cryptic V8/Next OOM crashes).
  - Never evicts an active turn, pending approval/question, or job with live background child processes (`ActiveProcesses > baseline`).
  - Soft ceiling at ~40% RAM or <15% available RAM; LRU eviction order: idle Edge tabs (2 min) → idle Edge browser (5 min) → idle Stage Runners (10 min) → idle `next dev` (20 min) → idle agent workers (10 min).

---

## 4. Primary Sources Index

All technical claims are grounded in primary documentation and verified locally against `codex-cli 0.171.0`, `claude 2.1.330`, and Edge/WebView2 `158` on Windows 11. Full categorized source links live in:
- [`docs/research/01-runtime-and-protocols.md` §6](research/01-runtime-and-protocols.md#6-primary-source-reference-index) (ACP spec & Rust SDK, Claude Code docs, Codex App Server & config, Antigravity CLI/ACP/MCP, `rmcp`, Tauri v2 Channels, SQLite WAL, Win32 Job Objects, `process-wrap`, `git-worktree`)
- [`docs/research/02-design-canvas-and-frontend.md` §11](research/02-design-canvas-and-frontend.md#11-sources) (Tauri/wry/WebView2 source & PRs, CDP Page/Emulation PDL, Vite 8, `vite-plugin-storybook-nextjs`, Tailwind Oxide scanner & `@source`, Next 16 lockfile & `allowedDevOrigins`, React 19 `createRoot`/`captureOwnerStack`/`useSyncExternalStore`, Oxc transformer, Streamdown, TanStack Virtual, MCP 2025-11-25/2026-07-28 & MCP Apps, Agent Skills spec)
