# Gantry — Hard-Stuff-First Implementation Roadmap

> **Philosophy:** Retire every risky platform, protocol, and bundler assumption with runnable spikes *before* building product UI on top of them. Every spike outputs runnable code, raw protocol fixtures (for deterministic offline unit tests), and real Windows RSS/latency measurements.

---

## Week 0: Risk-Retiring Spikes (Gate Before Phase 1)

**X1 runs first** (everything depends on the Windows process spine). **X2–X6, S1–S2, and S4** run in parallel right after X1 passes.

| Spike | What We Prove | Concrete Pass Criteria |
| :--- | :--- | :--- |
| **X1: Windows Process Spine** | `gantry-proc::ManagedCommand` using `PROC_THREAD_ATTRIBUTE_JOB_LIST` + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` + `CREATE_NO_WINDOW`, real `.exe` resolver + registry PATH merge, `dunce::canonicalize`. | `taskkill /F` Gantry while hosting `bun dev`→Vite, `next dev`, headless Edge (nested sandbox jobs), and a sandboxed Codex command → **0 surviving processes, 0 console flashes**. |
| **X2: Claude Warm Worker** | Persistent `claude.exe` (`stream-json` + `permission-prompt-tool stdio`), `--append-system-prompt-file`, `--mcp-config <file>` (`${GANTRY_MCP_TOKEN}` expansion), `--plugin-dir` skill namespacing, `DISABLE_AUTOUPDATER=1`. | 5 consecutive turns; mid-tool `interrupt`; `can_use_tool` for Bash, `AskUserQuestion`, `ExitPlanMode` across every permission mode; `control_cancel_request`; mid-turn `user` frame behavior verified; cross-cwd `--resume --fork-session` verified. Golden NDJSON fixtures saved. |
| **X3: Codex App-Server** | `codex.exe app-server` v2 handshake (`experimentalApi: true`), `-c mcp_servers.gantry.*` (`tool_timeout_sec=180`, `default_tools_approval_mode="auto"`, `bearer_token_env_var`), exhaustive 10-method `ServerRequest` handler (incl. `mcpServer/elicitation/request`), `turn/steer` + `clientUserMessageId`, `skills/extraRoots/set`. | End-to-end turn + steer race + elicitation + `requestUserInput` + `localImage` attachment + `thread/fork` + resume. Golden JSON-RPC fixtures saved. |
| **X4: Antigravity (`agy acp`)** | Official standalone `agy.exe acp --process-per-session --disable-telemetry` via `agent-client-protocol` Rust SDK inside a dedicated `LocalSet` actor. | Verify `initialize` capabilities (`mcpCapabilities.http`, `loadSession`, `promptCapabilities.image`), whether `agy` sends an `Origin` header, tool-result image passthrough, and `session/cancel`. |
| **X5: `rmcp` v3.x Local MCP Server** | `rmcp` Streamable HTTP on `127.0.0.1:0` with `axum` Origin-reject + constant-time Bearer middleware, dummy `echo_image` tool returning `outputSchema`/`structuredContent`, PNG `image`, and SSE progress notifications. | All installed providers (Claude, Codex, and `agy` once installed) discover tools, stream progress, and receive structured + image results with zero global config mutations. |
| **S1: Tauri v2 Iframe Security & Theming** | Cross-port `127.0.0.1` iframes (Tier 1 opaque `sandbox="allow-scripts"` with `ACAO: *` + Tier 2 `allow-same-origin`) inside Tauri v2 WebView2. | Zero IPC access (`__TAURI_INTERNALS__` invokes rejected even if script injected); per-frame `color-scheme` propagation works without touching Gantry's `data-theme`; WebView2 `WebResourceRequested` strips `X-Frame-Options`/`frame-ancestors` on `localhost:*`; Evergreen Runtime version check `>= 123.0.2420.47` verified. |
| **S2: VerifyEngine (Headless Edge CDP)** | System `msedge.exe` (`--headless=new --remote-debugging-port=0 --user-data-dir=...`) inside app-scoped Job Object, `DevToolsActivePort` discovery, `Emulation.setDeviceMetricsOverride` + `setEmulatedMedia`, settle protocol, diagnostics + Rust `image` downscale. | Warm p50 snapshot < 300 ms; catches intentional compile/runtime/console/overflow errors cleanly at 390×844 & 1440×900 in both light & dark modes. |
| **X6: Worktree × Tier 2 Lifecycle** | Full `Workspace` lifecycle on `vite-hono-base` and `hono-trpc-tanstack-base`: `git worktree add`, `.env*`/`.worktreeinclude` copy, auto-detected package manager install (`bun`/`pnpm`/`yarn`/`npm`) timing, Vite runner in worktree, ordered teardown (`notify` off → Edge tabs closed → jobs dead → `git worktree remove`). | First-try `git worktree remove` succeeds with Windows Defender active; RSS per workspace measured; package-manager detection verified across lockfile types. |
| **S4: `vite-harness` Prototype** | Project's own Vite 8 + config load + meta-framework plugin filter (`tanstack-start*`, `@tanstack/router-plugin`) + importer remap for `.gantry/` aliases + in-memory Tailwind v4 `@source` + SCSS `loadPaths` + `data-gantry-loc` Oxc pass. | Mockup in `.gantry/stage/` importing real `@components/*`, `@styles/*`, and Tailwind v4 utilities renders with HMR and zero files written outside `.gantry/`. |

---

## Week 1: Parallel Next.js Spikes (Run Alongside Phase 1 Start)

| Spike | What We Prove | Concrete Pass Criteria |
| :--- | :--- | :--- |
| **S3: `next-vite` on `pockethost-panel`** | `vite-plugin-storybook-nextjs` (pinned) + Next `sassOptions` mapping + `@pockethost/ui` kit + `next/font/google` offline cache + `UserProvider` scenario wrapper (no `RootDocument` `<html>` clash). | 3 real kit components render pixel-comparable to the live app; cold start < 5 s; zero network after font cache warm. |
| **S5: `next-attach` on `starfall-starter` / `metrichoard`** | `.next/dev/lock` discovery & stale-lock recovery, hash-guarded temporary `app/(gantry)/gantry-stage/<id>/page.tsx`, Tailwind v4 `@source` under Turbopack, `allowedDevOrigins` on `localhost:<port>`, `proxy.ts` redirect diagnostic, clean deletion on close/crash. | Async Server Component mockup renders in real root layout with HMR; `git status` stays clean before, during, and after close. |

---

## Phase 1: Hard Foundations + Complete Agent & Tier 1 Loop

**Goal:** A rock-solid daily-driver workbench for **Claude Code + Codex** with **Tier 1 standalone mockups**, full crash recovery, and zero Windows process leaks.

1. **Core Rust Backend:**
   - `gantry-proc` (`ManagedCommand`, single-instance guard, startup health check for `claude`/`codex`/`agy`/Git Bash/WebView2/longpaths)
   - `Workspace` manager (default Main Checkout + opt-in Worktree creation at `C:\Users\Chris\.gantry\wt\`, `.worktreeinclude`/`.env*` copy, lockfile-based package-manager auto-detection for `bun`/`pnpm`/`yarn`/`npm` + `postCreate` bootstrap FSM, ordered teardown, mutex-guarded `info/exclude`)
   - `gantry-db` (single-writer `rusqlite` WAL actor, full unified schema, `compacted_through_seq` delta compaction, boot crash recovery)
   - `ThreadCoordinator` FSM + `ClaudeStreamAdapter` + `CodexAppServerAdapter` (tested offline against X2/X3 golden fixtures)
   - `gantry-mcp` (`rmcp` v3.x Streamable HTTP on `P1`, Origin/Bearer guard, `stage_render`, `stage_update`, `stage_snapshot`, `stage_list`, `stage_close`, `gantry://guide`)
   - `gantry-tier1-oxc` + `StageHost` (`P2`) + `VerifyEngine` (headless Edge CDP)
   - Basic Resource Governor (`GlobalMemoryStatusEx` polling + LRU idle eviction)
   - `tauri-specta` v2 codegen (`packages/protocol`) + CI diff test
2. **React 19 Frontend (`apps/desktop`):**
   - Plain-TS `ThreadLog` external store + rAF coalescing + `useSyncExternalStore` + `@tanstack/react-virtual` chat timeline + `streamdown` (live item) + Shiki Web Worker (completed items)
   - Left rail (Projects → Workspaces/Threads with live status dots + Stages list)
   - Sticky approval & structured question cards above composer (keyboard-first `A`/`Shift+A`/`D`)
   - Inline PNG thumbnail stage cards in timeline + resizable/popout Stage Dock (viewport presets, zoom, light/dark toggle, console/diagnostics drawer, rev history scrubber)
   - Prompt addendum + `gantry-design-stage` `SKILL.md` injected cleanly into Claude (`--plugin-dir`) and Codex (`skills/extraRoots/set`)

---

## Phase 2: Real Project Components (Tier 2), Visual Feedback & Antigravity

**Goal:** Agents render real designs using your actual project components, you pin feedback directly onto rendered elements, and **Antigravity** joins as Provider #3.

1. **Tier 2 Harness Adapters:**
   - `vite-harness` (Vite 8 + TanStack Start + React Router 8 + Tailwind v4 + SCSS)
   - `next-vite` (Next 16 client components & workspace UI kits `@pockethost/ui`, `@airfield/ui`, offline Google Font cache)
   - `stage_setup` project analyzer + `.gantry/preview.tsx` & `.gantry/tsconfig.json` scaffolder
   - Immutable Tier 2 revision snapshots in `%LOCALAPPDATA%\gantry\stages\` (full rev scrubber + worktree-safe history)
2. **Design Review Loop:**
   - `stage_compare` (2–4 synchronized side-by-side variants A/B/C with pick/mix feedback)
   - `stage_inspect` + Stage Runtime inspector (`⌖` / hold `Alt`, `data-gantry-loc` + `ownerLoc` + React 19 fiber/ownerStack)
   - Click-to-pin comment popovers + WebView2 CDP element crops (640 px JPEG) attached to `composer_send` + `stage_feedback` MCP tool
   - Agent-led "Promote to codebase" flow running project `checks`
3. **Provider #3 & Workbench Ergonomics:**
   - `AntigravityAcpAdapter` (`agy acp --process-per-session --disable-telemetry`, `LocalSet` actor)
   - Cross-workspace thread fork & resume ("re-home" handling for Claude's cwd-slugged JSONL)
   - Git diff drawer (lazy-loaded), full Resource Governor tuning, and crash-recovery chaos tests

---

## Phase 3: Full-Fidelity Next.js RSC, Polish & Cross-Platform

1. `next-attach` adapter (single Gantry-owned `next dev` per project with LRU swap across workspaces) + `url` adapter (SolidStart / arbitrary routes)
2. Live visual tweak panel (token-step ephemeral style overlay → diff of intent to agent)
3. MCP Apps (`io.modelcontextprotocol/ui`) viewer + third-party MCP Apps host in the dock
4. Stage export (self-contained HTML/PNG design review bundle)
5. Optional embedded terminal tab (`xterm.js`, lazy-loaded) + macOS/Linux parity for `gantry-proc` and `VerifyEngine`
