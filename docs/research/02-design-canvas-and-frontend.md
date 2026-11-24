# 02 — Design Stage, MCP Tooling & Frontend Architecture

> **Status:** Research / design proposal, v1, 2026-11-24
> **Author role:** Design Canvas, MCP Tooling & Frontend Architect
> **Companion:** [`01-runtime-and-protocols.md`](01-runtime-and-protocols.md) (Rust backend, provider protocols, Job Objects, SQLite, MCP hosting)
> **Scope:** The Design Stage (Tier 1 standalone mockups + Tier 2 real-project component rendering), the local MCP toolset and agent Skill contract, Tauri v2/WebView2 iframe and screenshot plumbing, and the React 19 frontend.

Facts are backed by primary sources linked inline, and the full list is in §11. Three claims were **verified empirically on this machine** (Windows 11, Edge/WebView2 158, Node 24, bun 1.4) and are marked **🧪 Verified locally**. Anything that still needs a spike is marked **⚠ Spike**.

---

## 0. TL;DR: the decisions

| # | Decision | One-line rationale |
|---|---|---|
| D1 | **Headless Edge over CDP is the verification engine. The WebView2 iframe is only the human viewing surface.** | MCP tool calls must work with Gantry minimized, at exact viewports, in both themes, and return console/runtime/network/layout diagnostics. CDP gives all of that for free. 🧪 Warm screenshots took 73–283 ms with exact viewport + `prefers-color-scheme` emulation. No 130–217 MB Chromium download is needed on Windows. |
| D2 | **Tier 1 compiles in Rust (oxc) and serves native ESM + import map + vendored libs** from a loopback Stage Host. It runs in a `sandbox="allow-scripts"` opaque-origin iframe. Never a CDN. | ~0 ms toolchain spin-up, fully offline, no Node required. The opaque origin keeps it isolated from Tauri IPC and other stages. |
| D3 | **Tier 2 = Option C (hybrid), with adapters:** `vite-harness` (default; Vite and workspace UI kits) · `next-vite` (Next client components/kits via Next's own SWC + `next/*` mocks) · `next-attach` (full-fidelity RSC/pages inside the project's *own* `next dev`) · `url` (any framework, e.g. SolidStart). | the owner's real portfolio is Next 16 + SCSS + bun-monorepo UI kits plus Vite 8 apps. No single option covers it. The adapter boundary is the right abstraction. |
| D4 | **Reuse the project's own toolchain**: its `vite`, its `vite.config` (minus SSR/meta-framework plugins), its PostCSS/Tailwind/Sass, its React. Gantry ships only a fallback toolchain pack. | Dependency/React mismatch disappears by construction. Aliases are never re-derived. |
| D5 | **Mockups are real files** at `<appRoot>/.gantry/stage/<stageId>/*.tsx`, hidden from git via `$GIT_COMMON_DIR/info/exclude` (never `.gitignore`). Agents edit them with their **native file tools**. HMR updates the stage. | Agents are best at editing files, not at stuffing 300-line TSX into JSON tool args. You get zero `git status` noise and a trivial "Promote" step. |
| D6 | **Per-project `.gantry/preview.tsx` wrapper + `.gantry/stage.json`**, scaffolded by `stage_setup` (Gantry detects; the agent completes). | Providers, global CSS, fonts, `<html>` classes and mocks live in one explicit, versionable place, like `.storybook/preview.tsx`. |
| D7 | **Per-stage dark/light comes from `color-scheme` on the `<iframe>` element** (Chromium propagates it into the frame's `prefers-color-scheme`). Class/attribute strategies go through the wrapper's `theme` prop. | 🧪 Verified in Edge 158 on sandboxed opaque-origin iframes. Doesn't touch Gantry's own theme or the WebView2 profile. |
| D8 | **One local MCP server (Streamable HTTP, 127.0.0.1, per-thread bearer token) with 9 `stage_*` tools**, plus an MCP-instructions block, a cross-vendor **Agent Skill** (`SKILL.md`, agentskills.io), and a ~120-token prompt addendum. | All three agents (Claude Code, Codex, Antigravity) speak MCP-over-HTTP and the same SKILL.md format. Only the install path differs. |
| D9 | **Every render returns structured diagnostics** (`isError: true` on compile/runtime failure) so agents self-heal *before* the user looks. Snapshots return downscaled PNG `image` content plus layout QA. | The MCP spec says tool-execution errors should go back to the model for self-correction ([MCP tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)). |
| D10 | **Frontend: React never sees a token.** Rust batches per-thread `Channel` events (≤16 ms). A plain-TS `ThreadLog` store flushes per rAF to per-item `useSyncExternalStore` subscribers. The timeline is virtualized with no live iframes in it. | 500+ tok/s without jank. Webview reload is free via `seq` replay. |

---

## 1. Ground truth: what the owner's projects actually look like

I surveyed every `package.json` under `%USERPROFILE%\Desktop\Development\Personal` before designing anything. The brief assumed "Tailwind/shadcn with `@/*` aliases". **The real portfolio is different, and the design must fit the real portfolio:**

| Project | Framework | Styling | Aliases / structure | Footguns it exposes |
|---|---|---|---|---|
| `pockethost-panel` (+ `-install`, `PocketHost`) | **Next 16.4**, React 19.3, bun monorepo | Workspace kit **`@pockethost/ui`** (`packages/ui`, raw `.tsx` + `tokens.css` + `base.css`), Radix | `@/` in apps | **`packages/ui/fonts.ts` uses `next/font/google`** inside the kit. `RootDocument` renders `<html>`. Root layout is an **async server component** (`await getCurrentUser()`) feeding `UserProvider`. `DESIGN.md` + `lint:ui`/`lint:tokens` **forbid raw HTML/className in pages**. |
| `airfieldv2` | **Vite 8.3** + React 19.3 + **react-router 8**, bun monorepo, turbo | Workspace kit **`@airfield/ui`** with **SCSS** (`sideEffects: ["**/*.scss"]`), `@fontsource-variable/*` | `@` → app dir | **React Compiler via `@rolldown/plugin-babel`** in the Vite config. `envDir` at repo root. |
| `starfall-starter` / `starfall-kit` | **Next 16.4**, `typedRoutes`, `turbopack.root` = monorepo root | kit package `@starfall-kit/client` | — | `loadEnvConfig(root)` in `next.config.ts`. API rewrites to a Hono backend. |
| `metrichoard` | **Next 16.3** | **Tailwind v4 via `@tailwindcss/postcss`** *plus* SCSS modules; `components.json` (shadcn) | — | Mixed Tailwind + Sass in one app. |
| `vite-hono-base` | Vite 8.3 + React 19.2 | **Tailwind v4 via `@tailwindcss/vite`**, `@theme inline`, **dark mode via `@media (prefers-color-scheme: dark)`** | `@/`, `@utils`, `@config` in **both** `vite.config` and `tsconfig` | Dark mode can't be toggled by adding a class. You must emulate the media query. |
| `hono-trpc-tanstack-base`, `Notes` | **Vite 8 + TanStack Start 1.168** | **SCSS modules**, `css.preprocessorOptions.scss.loadPaths`, `@styles` alias | Vite 8 **native `resolve.tsconfigPaths: true`**; nested `src/tsconfig.json` with `"*": ["./*"]`, `@components/*` | `tanstackStart()` plugin turns the dev server into an SSR app with its own entry. Global CSS is imported as `?url` in `__root.tsx` `head()`. The router plugin **writes `routeTree.gen.ts`**. |
| `hono-next-base`, `SparkveyV6`, `sparkveyv5`, `offerable-*` | Next 15.3–16.4 | **SCSS modules**, `sassOptions.includePaths` | `@components/*` → **two** dirs (`_components`, `_loadingComponents`), `@public/*` → `../public` | Next root is `src/` (`next dev src/`), not the repo root. |
| `sparkvey-solidstart` | SolidStart 0.4 / Vite 4 | — | — | Not React. |
| `Sparkvey/Client` | Next 12 / React 17 | — | — | Legacy. Don't support it in the harness. |

Other facts that matter: **bun 1.4** is the package manager everywhere (`bun.lock`), Node 24 is installed, some packages use **TypeScript 7 (native `tsgo`)**, and lint is oxlint/oxfmt.

**Design consequences** (these drive everything below):

1. **SCSS is a first-class requirement**, not Tailwind-only. The harness must honor `sassOptions.includePaths` (Next) and `css.preprocessorOptions.scss.loadPaths` (Vite). It must resolve `sass` from the project, never bundle its own.
2. **The highest-leverage render target is the workspace UI kit** (`packages/ui`), not the Next app. Kits are plain client React plus CSS/SCSS plus tokens. They render perfectly in a Vite harness even when the consuming app is Next 16 + Turbopack. That's why Option C (hybrid) wins (§4).
3. **Aliases are not just `@/*`**. Multi-target paths (`@components/*` → 2 dirs), wildcard `"*": ["./*"]`, nested tsconfigs, and aliases defined only in `vite.config` must all work. The answer is: **reuse the project's own resolver config. Never re-derive aliases.**
4. **`next/font/google` inside kits must be shimmed**, and **offline** (cache font files in Gantry's app data on first fetch).
5. **Dark mode via `prefers-color-scheme`** must be toggleable per stage *without* touching the app's global color scheme (§5.6).
6. **House rules matter:** pockethost's `DESIGN.md` says pages compose kit components only. Agents must read and obey `DESIGN.md`/`AGENTS.md` when mocking. "Promote to codebase" must run the project's own checks (`bun run check`, `lint:ui`, `lint:tokens`).

---

## 2. Design Stage: system overview

### 2.1 Components

```
                ┌──────────────────────────── Gantry (Tauri v2 process) ────────────────────────────┐
Claude Code ─┐  │  MCP Server (axum, Streamable HTTP, 127.0.0.1:P1/mcp, bearer = per-thread token)   │
Codex ───────┼──►   └► StageService (Rust)  ── SQLite: stages, revisions, diagnostics, snapshots, pins  │
Antigravity ─┘  │        ├─ Tier1Compiler (oxc)          → writes immutable revs to app-data/stages/   │
                │        ├─ StageHost (axum 127.0.0.1:P2) → serves Tier 1 docs + runtime + stdlib       │
                │        ├─ RunnerSupervisor ──stdio JSONL──► Stage Runner (Node, per app root)        │
                │        │      (Job Object)                    vite-harness | next-vite               │
                │        │                                      (project's own vite, 127.0.0.1:P3)    │
                │        ├─ NextAttach  ──► user's/Gantry-owned `next dev` (localhost:3000)            │
                │        ├─ VerifyEngine ──CDP (pipe)──► headless msedge.exe (1 browser, N tabs)        │
                │        └─ GitHygiene (info/exclude, sweeps)                                         │
                │  Tauri commands + Channels ─────────────────────────────────────────────┐          │
                └──────────────────────────────────────────────────────────────────────────┼──────────┘
                                                                                           ▼
                 WebView2 (http://tauri.localhost) ── React UI ── Stage dock ── <iframe sandbox …>
                                                                   (P2: Tier 1, opaque origin)
                                                                   (P3 / :3000: Tier 2, cross-origin)
```

### 2.2 Lifecycle of one `stage_render` call

1. **Agent** calls `stage_render({file: ".gantry/stage/pricing/Pricing.tsx", viewports: ["mobile","desktop"]})`. Before that, it wrote the file with its own Write/Edit tool.
2. **MCP server** maps the bearer token to `{threadId, projectId, cwd, provider}` and calls `StageService.render`.
3. **StageService** resolves the app (`.gantry/stage.json` or auto-detect), picks the adapter, and makes sure a runner is up. Cold start sends MCP progress notifications. It registers stage `pricing`, rev N, and gets back a URL: `http://127.0.0.1:P3/__gantry/stage/pricing?rev=N`.
4. **VerifyEngine** opens a CDP tab in headless Edge. It sets `Emulation.setDeviceMetricsOverride` and `Emulation.setEmulatedMedia`, then navigates. It collects `Runtime.exceptionThrown`, `Runtime.consoleAPICalled`, `Log.entryAdded` and `Network.loadingFailed`, and waits for the runtime's `settled` signal (§4.9). Then it evaluates `__gantry.collect()` for layout QA. With `snapshot`, it captures PNGs.
5. **StageService** persists the revision and diagnostics, emits `stage.revision` and `stage.diagnostics` on the UI channel, and writes a thumbnail for the timeline card.
6. **MCP result** returns a one-line summary, the diagnostics text, and `structuredContent` (+ `image` blocks if requested). On compile/runtime errors it returns `isError: true` and Gantry shows the stage as a collapsed "draft (failing)" card, so the user isn't shown broken work.
7. **UI** auto-opens the dock on the first *successful* rev of a stage in this turn (configurable). The live iframe loads the same URL. Subsequent agent file edits hot-update through Vite HMR, and the iframe and headless tab update in place.

---

## 3. Tier 1: zero-setup standalone mockups

**Goal:** an agent with no project context, or in any project, can show an HTML page, a Tailwind snippet or a self-contained React/TSX component in under ~150 ms after the tool call. Fully offline. No Node required.

### 3.1 Input kinds

| `kind` | Agent sends | Gantry does |
|---|---|---|
| `html` | A full document or a fragment | Fragments are wrapped in the Stage document template. Full documents get the Stage runtime `<script>` injected as the first child of `<head>`. |
| `jsx` / `tsx` | One or more files (`files: {"App.tsx": "...", "Card.tsx": "..."}`) plus `entry` (default `App.tsx`, default export) | Rust-side transform (oxc) → ESM modules served by the Stage Host, plus an import map of vendored libs. Mounted with React 19 `createRoot`. |
| `markdown` (bonus) | Markdown | Rendered with the same pipeline. Useful for "design spec" docs next to mockups. |

Every kind accepts `css` (extra stylesheet text), `tailwind: true | false | "v4-browser"` (default `true` for jsx/tsx, auto-detected for html when `class="` contains utility-looking tokens), `props` (JSON for the entry component), `theme`, and `viewports`.

### 3.2 Compile pipeline: Rust-side oxc, not a JS bundler

```
stage_render(kind:"tsx", files) ─► Rust StageService
   for each file:
     oxc_parser → oxc_semantic → oxc_transformer
        TypeScript strip (no type-check)
        JSX automatic runtime (importSource "react")      ← react/jsx-runtime from import map
        inject data-gantry-loc on host elements (custom oxc Traverse pass)
     oxc_codegen → JS + sourcemap (inline)
   rewrite relative specifiers  "./Card" → "./Card.tsx.js" (resolved against the files map)
   validate bare specifiers against the Tier-1 stdlib allowlist → else structured error
   write rev to  %APPDATA%/gantry/stages/<stageId>/<rev>/   (immutable, content-addressed)
   serve via Stage Host at  http://127.0.0.1:P2/t/<hostToken>/s/<stageId>/r/<rev>/index.html
   (shared non-secret assets: http://127.0.0.1:P2/_gantry/{runtime.js, base.css, lib/...})
```

Why this design:

- **oxc is a Rust crate**, so it runs in the Tauri backend in-process. There's no Node and no WASM spin-up. It handles TS strip and the JSX automatic runtime, and refresh transforms exist if we ever want Fast Refresh for Tier 1. See the [Oxc transformer guide](https://oxc.rs/docs/guide/usage/transformer), [JSX options](https://oxc.rs/docs/guide/usage/transformer/jsx) and the [`oxc_transformer` docs.rs page](https://docs.rs/oxc_transformer). Oxc's self-reported benchmarks put its transformer at ~3–5× SWC and ~20–50× Babel ([Oxc transformer alpha](https://oxc.rs/blog/2024-09-29-transformer-alpha)). For single-file mockups, any of them is "instant". The point is **no extra runtime to ship**. Pin an exact `oxc` version, because its option structs move between minors.
- **No bundling step.** Modules are served as native ESM. Bare specifiers are resolved by an **import map** pointing at vendored, pre-built ESM files shipped inside Gantry's resources. Browsers handle this natively and it's trivially debuggable.
- **No CDN, ever.** Every library the import map points at is built at *Gantry build time* (a `scripts/build-stage-stdlib.ts` using Rolldown/esbuild) into `resources/stage-stdlib/<name>@<version>/index.js`.

**Tier-1 stdlib (initial allowlist):** `react`, `react-dom`, `react-dom/client`, `react/jsx-runtime`, `react/jsx-dev-runtime` (19.x), `clsx`, `tailwind-merge`, `class-variance-authority`, `lucide-react`, `motion` (`motion/react`), `recharts`, `date-fns`, `@radix-ui/react-*` primitives (optional), `zod`. An unknown import returns a tool error with the allowlist and the advice to "inline it, or use Tier 2 (`target: "project"`)." The agent self-corrects on the next call.

### 3.3 Tailwind in Tier 1

Use the official **`@tailwindcss/browser`** v4 build, vendored (not loaded from jsDelivr). It compiles utilities in-page from the DOM and supports `<style type="text/tailwindcss">` for `@theme` blocks ([Tailwind Play CDN docs](https://tailwindcss.com/docs/installation/play-cdn)). Tailwind labels it development-only. That's exactly our use case: mockups, not production builds.

- The agent can pass `tailwindCss: "@theme { --color-brand: oklch(0.7 0.2 250); }"`. Gantry emits it as `<style type="text/tailwindcss">`.
- **Ready signal waits for Tailwind.** The browser build observes mutations and injects a style tag. The runtime waits for two `requestAnimationFrame`s after the last style mutation before declaring `ready`, so snapshots never capture unstyled flashes.
- Tailwind v3 syntax (`tailwind.config` with `theme.extend`) is *not* supported in Tier 1. The skill tells agents to use v4 `@theme`. A project on v3 uses Tier 2, which runs the project's real Tailwind.

### 3.4 Document template

```html
<!doctype html>
<html lang="en" data-gantry-stage="{stageId}" data-gantry-rev="{rev}">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="
      default-src http://127.0.0.1:P2; script-src http://127.0.0.1:P2 'unsafe-inline' 'unsafe-eval' blob:;
      style-src http://127.0.0.1:P2 'unsafe-inline'; img-src http://127.0.0.1:P2 data: blob:; font-src http://127.0.0.1:P2 data:;
      connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'">
  <script src="/_gantry/runtime.js"></script>          <!-- first: error capture before anything runs -->
  <script type="importmap">{ "imports": { "react": "/_gantry/lib/react@19.2.x/index.js", ... } }</script>
  <link rel="stylesheet" href="/_gantry/base.css">     <!-- neutral reset + Gantry theme vars (opt-out) -->
  <script src="/_gantry/lib/tailwindcss-browser@4.x/index.js"></script>
</head>
<body><div id="gantry-root"></div>
  <script type="module" src="./entry.js"></script>
</body></html>
```

Notes:

- The CSP uses the explicit Stage Host origin instead of `'self'`. The document has an *opaque* origin (sandboxed, §5.2), and we don't want to depend on how each engine resolves `'self'` for sandboxed documents. Every module/asset response carries `Access-Control-Allow-Origin: *`, because module fetches from an opaque origin are CORS requests with `Origin: null` (§5.2).
- `connect-src 'none'` is the default. Mockups don't phone home. **This is what makes "offline-first" real:** a mockup that tries `fetch('https://api.example.com')` produces a CSP diagnostic the agent sees, rather than silently hanging. An agent can request `network: "allow"` (UI shows a badge).
- `'unsafe-eval'` is needed by the Tailwind browser build. The frame is sandboxed and cross-origin, so this is acceptable (§5).
- **Theme variables:** `/_gantry/base.css` defines neutral `--gantry-bg/--gantry-fg/...` tokens for both schemes. When the agent is designing *for a project* but using Tier 1 (quick sketch), `stage_render` accepts `tokensFrom: "project"`. Gantry reads the project's token CSS (the `:root { --… }` and `@theme` blocks it already discovered in project analysis, §4.4) and inlines it. That gives cheap visual consistency without running the project.

### 3.5 Inline vs expanded rendering

- **Timeline card (inline):** a static PNG thumbnail from the auto-snapshot (§6.4) plus a status line. Never a live iframe (§7.3, rule 8).
- **Dock (expanded):** live iframe. Toolbar: viewport presets **Mobile 390×844 · Tablet 820×1180 · Laptop 1280×800 · Desktop 1440×900 · Full (fill pane)**, custom W×H, zoom (fit / 50 / 75 / 100 %), light/dark/both, inspector toggle, console badge, reload, "open in popout", rev history scrubber (`◀ rev 3/5 ▶`), and compare (§6.3).
- **Viewport emulation in the live pane** is a fixed-size iframe (`width: 390px; height: 844px`) scaled with CSS `transform: scale()` to fit. That gives true CSS-pixel media queries at that width, which is what designers expect. "Full" mode uses `ResizeObserver` and the iframe fills the pane.
- **Auto-height for inline/fragment content:** the runtime posts `{type:"size", height}` via `ResizeObserver` on `document.documentElement`. The host sets iframe height, clamped at 4000 px.

---

## 4. Tier 2: real-project component rendering (the hard part)

**Goal:** the agent shows a design built from **the owner's actual components, tokens, CSS/SCSS/Tailwind, fonts and aliases**, rendered by **the project's own toolchain**, isolated from Gantry's React, with errors returned to the agent.

### 4.1 The options, honestly compared

| Criterion | **A. Managed Vite harness** (Storybook-nextjs-vite / Ladle / Cosmos style) | **B. Inject into the project's own dev server** (preview route) | **C. Hybrid adapters** ✅ |
|---|---|---|---|
| Vite projects (vite-hono-base, airfieldv2, tanstack-base) | ✅ Native: same Vite 8, same config, same plugins | ✅ Possible, but needs a route per router flavor (react-router 8, TanStack file routes, plain) | ✅ `vite-harness` |
| Workspace UI kits (`@pockethost/ui`, `@airfield/ui`) consumed by **Next** apps | ✅ with `next/*` shims (`next/font/google` is in the kit) | ⚠ Only via a Next route. Slow compile, full root layout | ✅ `next-vite` |
| Next pages / **async server components** / `next/headers` / server data | ❌ RSC only via experimental Suspense wrapping; server actions unsupported ([Storybook nextjs-vite docs](https://storybook.js.org/docs/get-started/frameworks/nextjs-vite)) | ✅ 100% fidelity | ✅ `next-attach` |
| Non-React (SolidStart) | ❌ (needs per-framework mount) | ✅ Frame an existing route | ✅ `url` adapter |
| Repo pollution | None (virtual entry + excluded `.gantry/stage`) | ⚠ Route files inside `app/` (excluded, but visible to `tsc`/`next build` while present) | Minimized: only `next-attach` writes route files, temporarily |
| Interferes with the user's dev server | No (separate port, separate process) | **Yes.** Next 16 allows **one `next dev` per project** (`.next/dev/lock`) ([Next 16 blog](https://nextjs.org/blog/next-16), [lockfile test](https://github.com/vercel/next.js/blob/canary/test/development/lockfile/lockfile.test.ts)) | Only `next-attach`, by attaching rather than spawning |
| Cold start | Vite 8 (Rolldown) ~1–3 s ⚠ Spike | Turbopack route compile ~1–5 s on first hit ⚠ Spike | Per adapter |
| Provider/CSS fidelity | Needs an explicit wrapper (`preview.tsx`) | Free: the real root layout | Wrapper for harnesses; free for attach |
| Implementation cost | Medium-high (config merging, plugin filtering, shims) | Medium (route codegen, lifecycle, proxy/auth) | High, but each adapter is independently shippable |

**Recommendation: C.** Ship `vite-harness` first, because it covers the Vite apps *and* is the substrate for `next-vite`. Then `next-attach`, because the owner is Next-heavy and page-level designs matter. `url` is nearly free. Don't build our own Next shims: **reuse `vite-plugin-storybook-nextjs`** (pinned) inside `next-vite`. It already loads `next.config`, runs Next's own SWC transform, aliases React to `next/dist/compiled/react*` (the same React the app uses), and mocks `next/headers`, `next/navigation`, `next/router`, `next/link`, `next/cache` and `next/dynamic`. It also handles `next/font/{google,local}` and `next/image` ([plugin source](https://github.com/storybookjs/storybook/blob/next/code/lib/vite-plugin-storybook-nextjs/src/index.ts), [next-mocks](https://github.com/storybookjs/storybook/tree/next/code/lib/vite-plugin-storybook-nextjs/src/plugins/next-mocks), [next-swc](https://github.com/storybookjs/storybook/blob/next/code/lib/vite-plugin-storybook-nextjs/src/plugins/next-swc/plugin.ts)).

### 4.2 Adapter interface

```ts
// Runs inside the Stage Runner (Node). Rust only sees the JSONL protocol (§8.3).
interface StageAdapter {
  id: 'vite-harness' | 'next-vite' | 'next-attach' | 'url';
  detect(app: AppInfo): DetectResult | null;           // confidence + reasons, used by stage_setup
  start(app: AppInfo, cfg: StageAppConfig): Promise<{ origin: string }>;   // e.g. http://127.0.0.1:52133
  addStage(s: StageSpec): Promise<{ url: string }>;    // registers virtual entry / writes route
  updateStage(s: StageSpec): Promise<void>;            // props/export/scenario changes (file edits = HMR)
  removeStage(id: string): Promise<void>;              // deletes route files for next-attach
  stop(): Promise<void>;
}
```

**Detection (in Rust, cheap, no Node):** read `package.json` deps and look for `vite.config.*`, `next.config.*`, `postcss.config.*`, `tsconfig.json` (follow `extends`), `components.json` (shadcn), and `.next/dev/lock`. Default choices:

| Signals | Default adapter |
|---|---|
| `vite` + `react` and no meta-framework | `vite-harness` |
| `vite` + `@tanstack/react-start` / `react-router` framework plugin | `vite-harness` (meta-framework plugins filtered, §4.5) |
| `next` | `next-vite` for `file` stages whose module graph is client-safe; **`next-attach`** for `url` stages, async server components, or when `next-vite` reports a server-only import |
| `solid-js`, `svelte`, `vue`, … | `url` (+ Tier 1 for sketches) |

### 4.3 Where mockups live, and git hygiene

```
<appRoot>/                                ← dir with the app's package.json (apps/web, src/ for hono-next-base…)
  .gantry/
    stage.json            (committed? → Q1 for the owner)  adapter + wrapper + theme + mocks + checks
    preview.tsx           (committed? → Q1)            providers/CSS/fonts wrapper
    mocks/…               (committed? → Q1)            module mocks (session, api client…)
    tsconfig.json         (generated)                  extends app tsconfig, includes ./**  (see 4.6)
    stage/                (ALWAYS local-only)          ephemeral mockups, one folder per stage
      pricing/Pricing.tsx
      nav-variants/Nav.tsx   (exports VariantA/B/C)
```

- **Never edit `.gitignore`.** Gantry appends `/.gantry/stage/` (and, for `next-attach`, the generated route dir) to **`$(git rev-parse --git-path info/exclude)`**. That's the documented home for "patterns specific to one user's workflow". It lives in `$GIT_COMMON_DIR` and is therefore **shared across all worktrees**, which is exactly what Gantry's worktree threads need ([gitignore docs](https://git-scm.com/docs/gitignore)).
- **Consequence 1 (Tailwind):** Tailwind v4's scanner respects git ignores, probably including `info/exclude` via the `ignore` crate. It *does* scan dot-folders ([Oxide scanner](https://github.com/tailwindlabs/tailwindcss/blob/main/crates/oxide/src/scanner/mod.rs), [detecting classes](https://tailwindcss.com/docs/detecting-classes-in-source-files)). So **excluded mockups are invisible to Tailwind unless we add `@source`** (fix in §4.7).
- **Consequence 2 (TS paths):** Vite 8's `resolve.tsconfigPaths` applies `paths` **only to files included by a tsconfig**, and TS's `**/*` doesn't match dot-folders ([Vite `resolve.tsconfigPaths`](https://vite.dev/config/shared-options#resolve-tsconfigpaths), [TS source](https://github.com/microsoft/TypeScript/blob/v5.9.3/src/compiler/utilities.ts#L9670)). So `@components/Button` **fails to resolve from `.gantry/stage/*`** out of the box (fix in §4.6).
- **Promote to codebase** is **agent-led**. The UI "Promote" button sends a structured message: *"Promote stage `pricing` (rev 7) to the codebase. Follow AGENTS.md/DESIGN.md conventions, move reusable parts into the kit if appropriate, then run: `bun run check`."* The project's `checks` from `stage.json` are listed. The agent moves the file, rewrites imports, runs checks, and the diff drawer opens. Gantry never mechanically copies files: house rules like pockethost's "pages compose kit components only" need judgment.
- **Sweeps:** on stage close, on app exit, and on startup, Gantry removes `next-attach` route files it created. They're recorded in SQLite with content hashes, and a file is only deleted if unmodified. `.gantry/stage/*` is retained for 14 days (configurable) so old revs can be reopened.

### 4.4 Project analysis (read-only, Rust)

`stage_setup` (MCP) and the first Tier 2 render run an analyzer whose report goes to the agent:

- **App roots:** a monorepo walk of bun/pnpm/npm `workspaces`, flagging apps that depend on workspace UI packages.
- **Styling:** Tailwind v3 (`tailwind.config.*` + `postcss.config`) vs v4 (`@import "tailwindcss"` in CSS; `@tailwindcss/vite` or `@tailwindcss/postcss`), SCSS (`sass`/`sass-embedded`, `sassOptions`, `preprocessorOptions`), CSS Modules, global CSS entry files (found by scanning root layout / `main.tsx` / `__root.tsx` imports).
- **Theme strategy:** `@media (prefers-color-scheme` in CSS → `media`; `.dark`/`@custom-variant dark` / `darkMode: 'selector'` / `next-themes` `attribute` → `class`/`attribute`.
- **Tokens:** `:root { --… }` and `@theme` blocks (also fed to Tier 1 `tokensFrom: "project"`).
- **Providers:** JSX providers in the root layout / `__root.tsx` / `main.tsx` (QueryClientProvider, ThemeProvider, UserProvider, RouterProvider…), marked server-data-dependent if they take props from `await`ed calls (pockethost's `UserProvider user={await getCurrentUser()}`).
- **Fonts:** `next/font/*`, `@fontsource*`, `@font-face`.
- **House rules:** presence of `DESIGN.md`, `AGENTS.md`, `CLAUDE.md`, and `check`/`lint:*` scripts.

### 4.5 `vite-harness`: how it actually works

**Process:** one **Stage Runner** (Node ≥20.19, required by Vite 8 ([Vite 8 announcement](https://vite.dev/blog/announcing-vite8))) per app root, launched by Rust with `node gantry-stage-runner.mjs --app <appRoot> --port 0`. It's long-lived while stages are open and stops after 10 min idle. Node is preferred over bun for Vite stability, with bun as fallback if no Node is found (⚠ Spike: verify Vite 8 under bun 1.4 on Windows).

**Vite resolution, which is the React-isolation guarantee:**

```js
const req = createRequire(path.join(appRoot, 'package.json'));
const vite = await import(pathToFileURL(tryResolve(req, 'vite') ?? toolchainPack('vite')));  // project's Vite first
const loaded = await vite.loadConfigFromFile({ command: 'serve', mode: 'development' }, undefined, appRoot);
```

- React, react-dom and every component resolve **from the project** (`resolve.dedupe: ['react','react-dom']`). Gantry never ships React into Tier 2. The iframe is a different origin and process from Gantry's UI, so there's never a shared React instance. That's guaranteed by the iframe boundary, not by convention.
- **Plugin filtering:** load the user's config, then drop plugins whose `name` matches a denylist of meta-framework/SSR/codegen plugins, because they hijack the server entry or **write files into the repo**:
  - `tanstack-start*` / `@tanstack/start*` and `@tanstack/router-plugin` (writes `routeTree.gen.ts`)
  - `react-router` / `@react-router/dev`, `vinxi*`, `nitro*`, `solid-start*`, `vike*`, `@cloudflare/vite-plugin`, `vite-plugin-pwa`
  - anything with `apply: 'build'`

  Keep everything else: `@vitejs/plugin-react` (incl. React Compiler via `@rolldown/plugin-babel` as in airfieldv2), `@tailwindcss/vite`, svgr, etc. The denylist is data in Gantry, and `stage.json` can override (`"plugins": {"drop": [...], "keep": [...]}`).
- **Merged inline config** (Gantry's overrides win):

```js
const config = vite.mergeConfig(userConfigWithoutDroppedPlugins, {
  configFile: false,
  root: appRoot,                                  // NOT .gantry: @tailwindcss/vite scans from config.root
  server: { host: '127.0.0.1', port: 0, strictPort: false, hmr: { host: '127.0.0.1' },
            fs: { allow: [vite.searchForWorkspaceRoot(appRoot), gantryRuntimeDir] },
            open: false, cors: false },
  resolve: { dedupe: ['react', 'react-dom'], tsconfigPaths: userHasTsconfigPaths ?? true },
  optimizeDeps: { entries: [], include: [...stageDepsFromImports] },  // don't crawl the app's index.html
  plugins: [gantryStagePlugin(stageRegistry)],      // first in order; enforce:'pre' hooks
  clearScreen: false, logLevel: 'info', customLogger: jsonlLogger,
});
```

  `fs.allow` must include `searchForWorkspaceRoot`, or monorepo kits break, since setting `fs.allow` disables auto-detection ([server options](https://raw.githubusercontent.com/vitejs/vite/main/docs/config/server-options.md)). `.env*` and `**/.git/**` stay denied by Vite's default `fs.deny`.

**`gantryStagePlugin` responsibilities:**

1. **Routes** (via `configureServer` middleware, pre-internal):
   - `GET /__gantry/stage/:id` → harness HTML.
   - `/__gantry/runtime.js` → the Stage Runtime.
   - Everything else is Vite's.
   - A **token check** applies on `/__gantry/*`: `?t=<runnerToken>` sets a `SameSite=Strict` cookie for the frame.
2. **Virtual entry** `\0gantry:entry/<id>` ([virtual module convention](https://vite.dev/guide/api-plugin#virtual-modules-convention)):

```ts
import '/__gantry/runtime-client.ts';                 // MUST be first: installs error hooks + DevTools hook before React loads
import * as Preview from '/.gantry/preview.tsx';      // wrapper, global CSS, fonts
import * as Mod from '/.gantry/stage/pricing/Pricing.tsx';
import { mountReact } from 'gantry:mount/react';
mountReact({ Preview, Mod, exportName: 'default', props: {/* from spec */}, scenario: 'default', stageId: 'pricing' });
```

3. **Importer remapping for aliases:** for any *non-relative* specifier imported from a file under `.gantry/`, `resolveId` calls `this.resolve(source, <appRoot>/src/__gantry_virtual_importer__.tsx, {skipSelf: true})`. Mockups therefore resolve `@components/*`, `@/`, `@styles/*`, `"*": ["./*"]`-style bare paths, and `vite.config` aliases **exactly as app code does**. Relative imports inside a multi-file mockup stay relative.
4. **Tailwind `@source` injection:** in a `transform` hook (`enforce: 'pre'`) on the app's Tailwind entry CSS (detected: contains `@import "tailwindcss"` or `@tailwind`), append `@source "<abs>/.gantry/stage";` **in memory**. This makes excluded mockups visible to v4 scanning ([`@source` docs](https://tailwindcss.com/docs/detecting-classes-in-source-files)). v3: add the glob to `content` via the PostCSS plugin options in the merged config.
5. **`data-gantry-loc` injection:** an AST pass (Vite 8's built-in Oxc parser via `this.parse`, edits via MagicString, no Babel) over `.gantry/**` and (opt-in) `src/**` adds `data-gantry-loc="rel/path.tsx:line:col"` to host JSX elements. This is dev only and in memory (same idea as `code-inspector-plugin` ([repo](https://github.com/zh-lx/code-inspector))). React 19 removed `_debugSource` (§4.8).
6. **Error relay:** `server.ws`/`hot` `vite:error` payloads and transform/resolve errors go to the runner logger, which emits diagnostics JSONL to Rust ([Vite HMR API](https://vite.dev/guide/api-hmr)).

**`mountReact` (runtime side):**

```tsx
const root = createRoot(el, {
  onUncaughtError: (e, info) => report('react-uncaught', e, info.componentStack),
  onCaughtError:   (e, info) => report('react-caught', e, info.componentStack),
  onRecoverableError: (e, info) => report('react-recoverable', e, info.componentStack),
});
root.render(
  <GantryErrorBoundary>
    <Preview.Wrapper theme={theme} viewport={vp} scenario={scenario}>
      <Suspense fallback={<GantrySuspenseMarker/>}>
        <Component {...props} />
      </Suspense>
    </Preview.Wrapper>
  </GantryErrorBoundary>
);
```

`createRoot` error callbacks are React 19 APIs ([react.dev createRoot](https://react.dev/reference/react-dom/client/createRoot)). `Preview.document` (html/body class + attrs) is applied to `document.documentElement`/`body` **before** first render. That matters because pockethost's kit puts `fontVariables` on `<html>`, and `RootDocument` renders `<html>` itself, so it **can't** be mounted in a div.

### 4.6 TS paths and editor/tsc correctness for mockups

The importer-remap (4.5 #3) makes Vite resolve correctly. Agents also run `tsc`/`tsgo` and their LSP, so `stage_setup` writes `.gantry/tsconfig.json`:

```jsonc
{ "extends": "../tsconfig.json",            // or ../src/tsconfig.json for hono-next-base-style nested configs
  "include": ["./**/*", "../next-env.d.ts", "../**/*.d.ts"] }   // dot-dir is now explicitly included
```

`paths` in an extended config resolve relative to the config that declares them. This works for multi-target aliases like `@components/*` → `_components` + `_loadingComponents`. ⚠ Spike: TS 7 (`tsgo`) parity.

### 4.7 The CSS/styling matrix (footguns → fixes)

| Setup in the owner's repos | What breaks naïvely | Fix in harness |
|---|---|---|
| **Tailwind v4 + `@tailwindcss/vite`** (vite-hono-base) | Mockup classes not generated (excluded folder); scan base wrong if `root` ≠ app | `root: appRoot` + in-memory `@source` append (4.5 #4) |
| **Tailwind v4 + `@tailwindcss/postcss`** (metrichoard, Next) | Same; `base` defaults to `process.cwd()` ([postcss plugin src](https://github.com/tailwindlabs/tailwindcss/blob/main/packages/@tailwindcss-postcss/src/index.ts)) | Runner `chdir(appRoot)` + `@source` append; Vite auto-loads the project's `postcss.config.*` |
| **Tailwind v3** (`tailwind.config` + `content`) | `content` globs exclude `.gantry` | Inject `content` glob through PostCSS plugin options ⚠ Spike (not in the owner's repos today; low priority) |
| **SCSS modules + `css.preprocessorOptions.scss.loadPaths`** (tanstack-base) | — works (we reuse the Vite config) | Nothing. Sass resolved from the project (`sass-embedded` > `sass`) ([Vite CSS options](https://vite.dev/config/shared-options#css-preprocessoroptions)) |
| **Next `sassOptions.includePaths`** (hono-next-base, SparkveyV6) | Vite doesn't read next.config Sass options. The Storybook plugin README says `sassOptions` is **not** supported ([README](https://github.com/storybookjs/storybook/blob/next/code/lib/vite-plugin-storybook-nextjs/README.md)) | `next-vite` maps `sassOptions.includePaths`/`loadPaths` → `css.preprocessorOptions.scss.loadPaths` and `additionalData` → `additionalData` |
| **Sass `@use '@styles/variables'`** via alias | — | Vite 8.1.1+ resolves tsconfig paths in CSS/Sass imports ([#22775](https://github.com/vitejs/vite/pull/22775)) |
| **Global CSS imported via `?url` in TanStack `head()`** | The harness doesn't run `head()`, so no global CSS | Wrapper imports it directly (`import '@styles/globals.scss'`); the analyzer pre-fills this |
| **`next/font/google` in a kit** (pockethost `fonts.ts`) | Needs network; build-time-only API | `next-vite` font plugin; **offline: Gantry caches font CSS+woff2 under app data** on first fetch and serves from cache after. Storybook supports `NEXT_FONT_GOOGLE_MOCKED_RESPONSES` for mocked fetches ([docs](https://storybook.js.org/docs/get-started/frameworks/nextjs-vite)) |
| **`@fontsource-variable/*`** (airfieldv2) | — | Works as-is |
| **Dark mode via `@media (prefers-color-scheme)`** (vite-hono-base) | Can't add `.dark` | `color-scheme` on the iframe element (D7, §5.6) + CDP emulation for snapshots |
| **Dark mode via class/attribute** (`next-themes`, `@custom-variant dark`) | — | Wrapper receives `theme` and sets the class/attr; `stage.json.theme.strategy` drives it ([Tailwind dark mode](https://tailwindcss.com/docs/dark-mode), [next-themes](https://github.com/shuding/next-themes)) |

### 4.8 Missing providers, server data, and the wrapper contract

`.gantry/preview.tsx`, scaffolded by `stage_setup` from the analyzer and completed by the agent (the skill has per-framework recipes):

```tsx
// .gantry/preview.tsx  (pockethost-panel/apps/client-panel)
import type { PreviewDocument, WrapperProps, Scenarios } from './gantry-env';   // .d.ts written by stage_setup
import '@pockethost/ui/styles/tokens.css';
import '@pockethost/ui/styles/base.css';
import { fontVariables } from '@pockethost/ui/fonts';        // next/font/google → shimmed by next-vite
import { UserProvider } from '@/lib/user';
import { mockUser } from './mocks/user';

export const document: PreviewDocument = { html: { className: fontVariables, lang: 'en' }, body: {} };

export const scenarios: Scenarios = {
  default:   { user: mockUser },
  loggedOut: { user: null },
  newUser:   { user: { ...mockUser, servers: [] } },
};

export function Wrapper({ children, scenario }: WrapperProps<typeof scenarios>) {
  return <UserProvider user={scenario.user}>{children}</UserProvider>;   // RootDocument is NOT used: it renders <html>
}
```

Rules the skill teaches:

- **Wrap with the app's real client providers.** Replace *server-fetched inputs* with **scenarios** (named mock data sets the agent can switch: `stage_render({..., scenario: "loggedOut"})`).
- **Module mocks** for server-only or network modules go in `stage.json.mocks` (`"@/lib/session": ".gantry/mocks/session.ts"`). They're applied via `resolve.alias` in the harness only. `server-only` → empty module. `next/headers` → Storybook mock.
- **Routers:** components using `<Link>`/`useNavigate` from TanStack Router or react-router get wrapped in a **memory router** by the wrapper (recipes in the skill: TanStack `createRouter({ history: createMemoryHistory() })` with a root route rendering `children`; react-router `MemoryRouter`). `next/link`/`next/navigation` are mocked by `next-vite`.
- **QueryClient:** fresh `QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } })`, seeded from scenario data with `setQueryData`. Real network is off by default (Q4 for the owner).
- **React 19 has no `_debugSource`.** React removed `__source`/`_debugSource` in [PR #28265](https://github.com/facebook/react/pull/28265). Component *names* come from fibers via a DevTools-hook install before React loads (the [`bippy`](https://github.com/aidenybai/bippy) approach, which `react-grab` uses ([repo](https://github.com/aidenybai/react-grab))), and `captureOwnerStack()` in dev ([react.dev](https://react.dev/reference/react/captureOwnerStack)). *Locations* come from our `data-gantry-loc` transform.

### 4.9 Error capture and the "settled" contract (what the agent gets back)

The **Stage Runtime** (≈8 KB, injected first in every stage document, both tiers and all adapters) plus the CDP listeners produce:

```ts
interface StageDiagnostics {
  status: 'ok' | 'warnings' | 'error';
  phase: 'resolve' | 'compile' | 'runtime' | 'render' | 'settled' | 'timeout';
  build:   { file: string; line?: number; col?: number; message: string; frame?: string; plugin?: string }[];
  runtime: { kind: 'error' | 'unhandledrejection' | 'react-uncaught' | 'react-caught' | 'react-recoverable';
             message: string; stack?: string /* source-mapped */; componentStack?: string; ownerStack?: string }[];
  console: { level: 'error' | 'warn' | 'info' | 'log'; message: string; count: number }[];   // deduped, capped 30
  network: { url: string; status: number | 'failed' | 'blocked-csp' | 'blocked-mock'; type: string }[];
  layout: {
    viewport: { w: number; h: number }; document: { w: number; h: number };
    horizontalOverflow: { selector: string; loc?: string; overflowPx: number }[];   // #1 mobile bug agents ship
    clippedText: { selector: string; loc?: string }[];                               // text-overflow / line-clamp hits
    emptyRender: boolean;                                                            // root has no visible box
    brokenImages: string[]; fontsNotLoaded: string[];
    tinyTapTargets?: { selector: string; w: number; h: number }[];                   // mobile only, <24px
    contrast?: { selector: string; ratio: number; required: number }[];              // opt-in (axe-core subset)
  };
  timings: { compileMs?: number; firstRenderMs?: number; settledMs: number };
}
```

**Settled** means: React committed (a `useLayoutEffect` in the mount root fires) **and** `document.fonts.ready` **and** all `<img>` decoded or failed **and** no Vite HMR update pending **and** the MutationObserver has been quiet for 150 ms (Tailwind browser build / Suspense) **and** two rAFs have passed. Cap: 8 s, after which `phase: 'timeout'` comes with whatever was collected. Infinite animations don't block settling, because we only watch DOM mutations, not animation frames. For snapshots, the runtime additionally pauses CSS animations/transitions (`* { animation-play-state: paused !important; transition: none !important }` plus `document.getAnimations().forEach(a => a.pause())`) for determinism.

**Source-mapped stacks:** the runner maps `http://127.0.0.1:P3/.gantry/stage/...?t=...:line:col` frames back to workspace-relative paths using Vite's module graph/sourcemaps. Agents get `.gantry/stage/pricing/Pricing.tsx:42:9`, not URLs.

### 4.10 `next-attach` (full-fidelity Next: RSC, pages, real root layout)

- **Discover the dev server:** read `.next/dev/lock`. On current Next it holds `{pid, port, hostname, appUrl, startedAt}` ([lockfile test](https://github.com/vercel/next.js/blob/canary/test/development/lockfile/lockfile.test.ts)) ⚠ Spike: confirm on 16.4. If it's alive, **attach**. If not, offer **"Start dev server in Gantry"**, which runs the project's `dev` script under Neon #2's process supervisor (Job Object). Gantry then *owns* the one allowed `next dev`, and the owner's browser can use it too. The Next maintainers explicitly intend agents to **share** the dev server ([PR #100684](https://github.com/vercel/next.js/pull/100684)).
- **Route codegen:** write `app/(gantry)/gantry-stage/<stageId>/page.tsx`. The route group keeps URLs clean, and `_`-prefixed folders would be non-routable ([project structure](https://nextjs.org/docs/app/getting-started/project-structure)). The page statically imports the mockup from `.gantry/stage/<id>/` and renders it inside `<GantryStageClient>`, a client component that loads the Stage Runtime and applies `scenario` via context. Async server components in the mockup work because this is real Next.
- **Root layout applies.** That's the point (real providers, fonts, `RootDocument`). `frame: "bare"` renders the mockup in a `position: fixed; inset: 0; z-index: max` container with the app's background, hiding site chrome without touching the layout.
- **Footguns handled:**
  - **Auth proxy** (`proxy.ts`, formerly `middleware.ts` in Next 16) may redirect `/gantry-stage/*` to `/login`. The runtime detects the redirect and returns diagnostic `"redirected by proxy.ts to /login"`, with a suggested fix (matcher exclusion for `/gantry-stage`, or a dev-only bypass), which needs the user's consent (Q2).
  - **`allowedDevOrigins`:** Next blocks cross-origin requests to dev assets by default. Only localhost/subdomains and the startup hostname are allowed, matched on `Origin` hostname ([docs](https://nextjs.org/docs/app/api-reference/config/next-config-js/allowedDevOrigins)). So the iframe must load **`http://localhost:<port>`** (not `127.0.0.1` unless that's the startup host). It must use **`allow-same-origin`**: an opaque `Origin: null` frame would have its HMR/dev requests refused, and `null`-origin handling was the subject of a CVE ([CVE-2026-27977](https://advisories.gitlab.com/npm/next/CVE-2026-27977/)).
  - **Build/typecheck leakage:** excluded files still exist on disk. While a stage is open, `next build`/`tsc`/`lint:ui` see `app/(gantry)/…`. The generated page uses only components (no raw HTML, satisfying pockethost's `lint:ui`). Routes are deleted on stage close, and a banner warns if Gantry detects a `next build` starting while generated routes exist.
  - **`typedRoutes`** (starfall-starter) regenerates route types that include `/gantry-stage/*`. This is harmless and in `.next/`.

### 4.11 `url` adapter (any framework)

`stage_render({ url: "/dashboard" })` frames a route of the project's running dev server (attach via configured `devServer.url`, or start it). The full Stage Runtime can't be injected into an arbitrary app, so diagnostics come from **CDP only** (console, exceptions, network, layout via `Runtime.evaluate` of the runtime script into the page with `Page.addScriptToEvaluateOnNewDocument`). That's enough for SolidStart (`sparkvey-solidstart`) and any page-level QA. The inspector works in the headless tab and, for pins, via CDP-injected runtime in the in-app iframe ⚠ Spike: WebView2 per-frame script injection.

---

## 5. Tauri v2 + WebView2: security, origins, iframe plumbing and screenshots

### 5.1 Facts that constrain the design

1. **The app origin is `http://tauri.localhost`** on Windows by default. Custom schemes map to `http://<scheme>.localhost` (`useHttpsScheme` switches to https, and changing it later wipes storage) ([tauri-utils config.rs](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri-utils/src/config.rs), [wry custom_protocol_workaround.rs](https://github.com/tauri-apps/wry/blob/dev/src/custom_protocol_workaround.rs)).
2. **Tauri trusts any frame served from the app's own custom protocols as "Local".** IPC trust is decided from the *requesting frame's* URL. `is_local_url` is true for the tauri host, `devUrl`/`frontendDist`-relative URLs, **and any user-registered custom protocol** ([PR #9100](https://github.com/tauri-apps/tauri/pull/9100), [webview/mod.rs](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/webview/mod.rs)). Everything else is `Remote` and needs an explicit `remote.urls` capability ([capabilities](https://v2.tauri.app/security/capabilities/)).
3. **On Windows, Tauri's IPC init scripts (incl. the invoke key) likely reach *every* frame**, sandboxed ones included, because WebView2's `AddScriptToExecuteOnDocumentCreated` is all-frames. There's an open fix in [wry PR #1874](https://github.com/tauri-apps/wry/pull/1874).
4. **`srcdoc`, `blob:` and `data:` iframes inherit the parent's CSP** via the policy container ([HTML spec](https://html.spec.whatwg.org/multipage/browsers.html)). Tauri hashes/nonces `script-src`, which makes `'unsafe-inline'` ineffective ([Tauri CSP](https://v2.tauri.app/security/csp/), [manager/mod.rs](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/manager/mod.rs)). So **agent-written inline scripts in a `srcdoc` mockup would be blocked by Gantry's own CSP.**
5. **Custom-protocol iframes do work on WebView2 ≥ 1.0.2365.46** (wry 0.50.4, [PR #1491](https://github.com/tauri-apps/wry/pull/1491)), with an open bug for windows created via `NewWindowResponse::Create` ([wry #1916](https://github.com/tauri-apps/wry/issues/1916)).
6. **`CapturePreview` never completes for a hidden webview**, and true offscreen rendering isn't supported ([WebView2Feedback #579](https://github.com/MicrosoftEdge/WebView2Feedback/issues/579), [#547](https://github.com/MicrosoftEdge/WebView2Feedback/issues/547)).
7. **`PreferredColorScheme` is profile-wide.** It would flip Gantry's own UI too ([ICoreWebView2Profile](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2profile)).

**Conclusions:**

- ❌ **Never serve stage content from a Tauri custom protocol** (fact 2 makes it IPC-trusted).
- ❌ **Never use `srcdoc`/`blob:`** for stages (fact 4).
- ✅ **Serve all stage content from loopback HTTP servers on ports Gantry doesn't use for its UI.** That makes every stage frame `Origin::Remote` to Tauri.
- ✅ **Grant no `remote.urls` capability, ever.** Even if the invoke key leaks into a frame (fact 3), Tauri rejects the call by origin.

### 5.2 Origin and sandbox matrix

| Frame | Served by | URL shape | `sandbox` | Effective origin | Why |
|---|---|---|---|---|---|
| Gantry UI | Tauri | `http://tauri.localhost/` | — | tauri.localhost | — |
| **Tier 1** | Rust **Stage Host** (axum) | `http://127.0.0.1:P2/t/<hostToken>/s/<stageId>/r/<rev>/index.html` | `allow-scripts` (+ `allow-forms allow-modals allow-popups` optional) | **opaque (`null`)** | Max isolation between stages and from everything. Storage APIs throw, so the runtime installs in-memory `localStorage`/`sessionStorage` shims so mockups that touch them don't crash. |
| **Tier 2** `vite-harness`/`next-vite` | Stage Runner (Vite) | `http://127.0.0.1:P3/__gantry/stage/<id>?t=<runnerToken>` | `allow-scripts allow-same-origin allow-forms allow-modals allow-popups allow-downloads` | `127.0.0.1:P3` | HMR websocket, module CORS, localStorage, cookies all need a real origin. Safe, because it's cross-origin to Gantry and `Remote` to Tauri. |
| **Tier 2** `next-attach`/`url` | Project dev server | `http://localhost:3000/gantry-stage/<id>` | same as above | `localhost:3000` | `allowedDevOrigins` + HMR need a real, allowed origin (§4.10). |

Never grant `allow-top-navigation*` (a frame could navigate Gantry away) or `allow-popups-to-escape-sandbox`. Links with `target=_blank` are intercepted by the runtime and posted to the host, which opens them in the system browser after confirmation.

**Tier 1 opaque-origin footgun:** ES module scripts loaded by an opaque-origin document are **cross-origin requests with `Origin: null`**, so the Stage Host **must send `Access-Control-Allow-Origin: *`** on Tier 1 assets. Otherwise every `<script type="module">` fails silently. This is very likely the root cause of Tauri's documented "ES modules fail in sandboxed iframes on Windows" caveat ([isolation docs](https://v2.tauri.app/concept/inter-process-communication/isolation/)). Tier 1 assets are non-secret, and the capability token is in the path. ⚠ Spike to confirm.

### 5.3 Loopback server hardening (Stage Host, Runners, MCP)

- Bind **`127.0.0.1` only** (not `localhost`, which may resolve to `::1` first; not `0.0.0.0`).
- **Host header allowlist** (`127.0.0.1:P` / `localhost:P`) against DNS rebinding. Vite already enforces `server.allowedHosts` since the CVE-2025-24010 fix ([advisory](https://github.com/vitejs/vite/security/advisories/GHSA-vg6x-rcgg-rjx6)).
- **Unguessable tokens:** Stage Host uses a path token. Runner uses a query token → `SameSite=Strict` cookie. MCP uses `Authorization: Bearer` per thread.
- **MCP endpoint rejects any request that carries an `Origin` header** with 403. CLI agents don't send one, browsers always do. This stops a stage iframe (which *is* on 127.0.0.1) from calling Gantry's MCP tools, and satisfies the spec's MUST-validate-Origin rule ([Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)).
- Mockups have `connect-src 'none'` by default (Tier 1). Tier 2 runs the project's real code. The skill and wrapper default to mocked network (Q4).

### 5.4 Gantry's own CSP (tauri.conf.json)

```jsonc
"app": { "security": {
  "csp": {
    "default-src": "'self'",
    "script-src": "'self'",
    "style-src": "'self' 'unsafe-inline'",
    "img-src": "'self' data: blob: asset: http://asset.localhost",            // snapshot PNGs via asset protocol
    "frame-src": "http://127.0.0.1:* http://localhost:*",                     // stages only
    "connect-src": "ipc: http://ipc.localhost",
    "font-src": "'self' data:"
  },
  "freezePrototype": true,
  "capabilities": ["main"]           // local-only; NO "remote" entries anywhere
}}
```

Tauri only touches `script-src`/`style-src` when injecting hashes, so `frame-src` passes through untouched ([CSP docs](https://v2.tauri.app/security/csp/)).

### 5.5 `postMessage` bridge (host ⇄ stage runtime)

Origins are mixed (`null` for Tier 1), so authenticate by **window identity + per-frame nonce**, not by origin string:

```ts
// Host side (Gantry UI)
const ch = crypto.randomUUID();                         // per iframe instance
iframe.src = `${stageUrl}#gantry-ch=${ch}&host=${encodeURIComponent(location.origin)}`;
window.addEventListener('message', (e) => {
  if (e.source !== iframe.contentWindow) return;        // identity check (origin may be "null")
  const m = e.data; if (m?.gantry !== 1 || m.ch !== ch) return;
  dispatchStageMessage(stageId, m);
});
const send = (msg) => iframe.contentWindow!.postMessage({ gantry: 1, ch, ...msg }, '*');  // '*' required for opaque origins

// Stage runtime side
const { ch, host } = parseHash(location.hash);
addEventListener('message', (e) => { if (e.source !== parent || e.data?.ch !== ch) return; /* handle */ });
parent.postMessage({ gantry: 1, ch, type: 'hello', v: RUNTIME_VERSION }, host);  // explicit targetOrigin to Gantry
```

Messages (versioned, all JSON):
- **stage → host:** `hello`, `rendered`, `settled{diagnostics}`, `size{h}`, `error{…}`, `console{…}`, `hover{box,label}`, `pin{descriptor}`, `route{path}`, `scroll{y}`, `openExternal{url}`.
- **host → stage:** `config{inspector, theme, scenario}`, `highlight{selector}`, `scrollTo{y|selector}`, `collect{}`, `freeze{animations}`.

The **authoritative** diagnostics for MCP come from the headless verify tab (§5.7). The in-app bridge drives the human UX: badges, inspector, pins.

### 5.6 Dark/light and viewports in the live pane

- **Theme (media strategy):** set `iframe.style.colorScheme = 'dark' | 'light'`. Chromium makes the embedded document's `prefers-color-scheme` follow the embedding element's `color-scheme` ([Chrome dark-mode guide](https://developer.chrome.com/docs/modern-web-guidance/guides/visual-design/dark-mode), [CSSWG #13843](https://lists.w3.org/Archives/Public/public-css-archive/2026Apr/0698.html)). **🧪 Verified locally (Edge 158):** sandboxed, opaque-origin `data:` iframes with `style="color-scheme:dark"` matched `prefers-color-scheme: dark` in both CSS and `matchMedia`, while sibling frames with `light`/unset matched light, all under a page emulated as light. Safari behaves differently, which doesn't matter on WebView2. ⚠ Spike: confirm inside Tauri's WebView2 with cross-port iframes.
- **Theme (class/attr strategy):** the host sends `config{theme}` and the wrapper applies `.dark`/`data-theme`. The iframe `color-scheme` is set too, so UA widgets match.
- **Gantry's own UI** uses `data-theme` on its root and never `prefers-color-scheme`, so nothing about stage theming can leak into it.
- **Viewport presets:** fixed CSS-pixel iframe size, scaled via `transform: scale(k)` to fit the dock. Media queries see the true width. A "Device frame" toggle is cosmetic only.

### 5.7 Screenshots: evaluation and decision

| Approach | Works hidden/minimized | Exact viewport + DPR | Theme emulation | Full-page | Diagnostics | Cost/footprint | Verdict |
|---|---|---|---|---|---|---|---|
| `ICoreWebView2::CapturePreview` | ❌ never completes when not visible ([#579](https://github.com/MicrosoftEdge/WebView2Feedback/issues/579)) | ❌ whatever's on screen | ❌ | ❌ viewport only ([docs](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2)) | ❌ | Free | Only for "what I see" crops, if at all |
| WebView2 **CDP** (`CallDevToolsProtocolMethod` → `Page.captureScreenshot` with `clip`) on the main UI webview | ⚠ needs visible window | ❌ bound to the dock layout | ⚠ emulation is page-wide (would hit Gantry UI); OOPIFs are separate targets ([ForSession docs](https://learn.microsoft.com/en-us/dotnet/api/microsoft.web.webview2.core.corewebview2.calldevtoolsprotocolmethodforsessionasync)) | ❌ | Partial | Free | ✅ **Pin crops** of exactly what the user saw |
| Hidden 2nd Tauri `WebviewWindow` | ❌ (`IsVisible=false` ⇒ not rendered) | — | — | — | — | — | ❌ |
| Off-screen-positioned visible window (the WebView2 #579 workaround) | ⚠ hacky, taskbar/focus quirks | ✅ | ✅ (own profile) | ✅ via CDP | ✅ | RAM: another renderer | Fallback only |
| **Headless system Edge via CDP** | ✅ | ✅ `Emulation.setDeviceMetricsOverride` | ✅ `Emulation.setEmulatedMedia` ([Emulation.pdl](https://github.com/ChromeDevTools/devtools-protocol/blob/master/pdl/domains/Emulation.pdl)) | ✅ `captureBeyondViewport` ([Page.pdl](https://github.com/ChromeDevTools/devtools-protocol/blob/master/pdl/domains/Page.pdl)) | ✅ Runtime/Log/Network domains | **0 MB download on Windows 11.** One `msedge.exe` process tree (~150–300 MB RSS while active ⚠ measure) | ✅ **Primary** |
| Playwright + bundled Chromium | ✅ | ✅ | ✅ | ✅ | ✅ | **130 MB** headless-shell / **217 MB** full Chrome for Testing download + a Node dependency ([browsers.json](https://github.com/microsoft/playwright/blob/main/packages/playwright-core/browsers.json)) | ❌ unnecessary on Windows. Keep `chrome-headless-shell` as the **optional** non-Windows fallback |
| `html-to-image`/`modern-screenshot` in-page | ✅ | ⚠ | ⚠ | ✅ | ❌ | Free | ❌ Re-implements rendering. Breaks on cross-origin images, fonts, backdrop-filter. Not trustworthy for visual QA |

**🧪 Verified locally (this machine, Edge 158, Node 24 raw CDP over WebSocket):**
- Cold browser launch to CDP-ready: **531 ms**.
- Warm navigate + emulate + capture: **283 ms** first, then **100 ms** (1440×900 light) and **73 ms** (390×844 light).
- `prefers-color-scheme` emulation is correct per capture.
- **Footgun found:** the CLI path `--headless=new --screenshot --window-size=390,844` produced a **518 px layout viewport** while saving a 390 px image. *Never* trust window-size flags. Always use `Emulation.setDeviceMetricsOverride`.
- **Second footgun:** with `mobile: true`, pages lacking `<meta name="viewport">` lay out at **980 px**. Our Tier 1 template always includes it. For Tier 2/`url`, the analyzer flags a missing viewport meta as a diagnostic.

**VerifyEngine design (Rust):**
- Locate Edge: `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe`, falling back to known paths. Then try Chrome. Then offer a one-time `chrome-headless-shell` download (EEA users can uninstall Edge).
- Launch `--headless=new --remote-debugging-pipe --user-data-dir=<appdata>/gantry/verify-profile --no-first-run --disable-extensions --mute-audio`. Pipe transport means no TCP port, so other local processes can't attach. It runs inside Neon #2's Job Object.
- Drive it with **`chromiumoxide`** ([docs.rs](https://docs.rs/crate/chromiumoxide)), whose `chrome_executable` points at msedge, or with a thin hand-rolled CDP client (~400 LOC; we only need ~15 methods).
- **One browser, one tab per active stage.** Tabs stay alive so HMR updates arrive without reloads. **Idle-kill the browser after 5 min.**
- Snapshot pipeline:
  1. `setDeviceMetricsOverride` (DPR default 1; 2 for "retina" requests)
  2. `setEmulatedMedia`
  3. runtime `freeze`
  4. wait settled
  5. `captureScreenshot`, either `clip` by selector box or `captureBeyondViewport` capped at 4× viewport height
  6. **downscale in Rust** (`image` crate) to the model budget (§6.4)
  7. write PNG to `app-data/stages/<id>/snapshots/`

### 5.8 Popout and multi-window

The Stage dock can pop out into a second `WebviewWindow` loading Gantry's own `/stage-popout?stageId=` route. That's the same origin and the same React bundle, just a different layout. Both windows subscribe to the same `StageEvent` channel. Avoid multi-webview `add_child`: it's behind the `unstable` feature and deadlocks from sync commands on Windows ([window/mod.rs](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/window/mod.rs)). Iframes inside our React tree are simpler and fully controllable.

---

## 6. First-class agent ergonomics: MCP toolset + Skill + prompt contract

### 6.1 The MCP server

- **Transport:** Streamable HTTP at `http://127.0.0.1:P1/mcp`, hosted in-process by Rust (Neon #2: `rmcp` or hand-rolled on axum).
- **Protocol versions:** support **both** MCP `2025-11-25` (initialize handshake, optional `MCP-Session-Id`) and **`2026-07-28`**, the current revision. It's stateless, with no `initialize` and no `Mcp-Session-Id`; it adds a `server/discover` RPC and a required `resultType` ([versioning](https://modelcontextprotocol.io/specification/versioning), [2026-07-28 changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)). Agents in the wild still ship older clients.
- **Identity = bearer token per Gantry thread.** It's minted when Neon #2 spawns the agent and maps to `{threadId, projectId, cwd, worktree, provider}`. This is how a tool call knows which thread's dock to update and which project root to render against. The 2026-07-28 design ("state via handles the model passes back") matches ours: `stageId` *is* the handle.
- **Origin header present → 403** (§5.3). Bind 127.0.0.1.
- **Long calls:** a cold Tier 2 runner can take seconds. Emit `notifications/progress` ("starting Vite for apps/web…", "optimizing deps…") and keep per-call budgets under agent defaults (Codex `tool_timeout_sec` defaults to 60 ([Codex MCP](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)), so Gantry sets it to 180 for this server). If the runner isn't ready by ~45 s, return `status: "building"` with the `stageId` and the hint "call `stage_snapshot` or `stage_update` to get results". Never block for minutes.
- **Output budgets:** Claude Code warns above 10k tokens and caps MCP output at `MAX_MCP_OUTPUT_TOKENS` (default 25k). Image results count toward it ([Claude Code MCP](https://code.claude.com/docs/en/mcp)). Default render results stay under ~1.5k tokens of text. Images are only returned when asked for.

### 6.2 Tool catalog (9 tools)

Common `StageSource`, exactly one of:

| Field | Tier | Meaning |
|---|---|---|
| `source: { kind: "html" \| "tsx" \| "jsx" \| "markdown", files: Record<string,string>, entry?: string, css?: string, tailwindCss?: string }` | 1 | Inline, self-contained. |
| `file: string` (+ `export?: string`, default `"default"`) | 2 | Path (workspace-relative) to a mockup module the agent wrote, normally under `.gantry/stage/<id>/`. |
| `url: string` | 2 | Route on the project's dev server (`"/dashboard"`). |

Common options: `app?` (monorepo app key or path; default from `stage.json`/cwd), `props?` (JSON), `scenario?` (wrapper scenario name), `viewports?` (`"mobile"|"tablet"|"laptop"|"desktop"|"full"|{w,h}`, default `["desktop"]`), `theme?` (`"light"|"dark"|"both"`, default the project default), `frame?` (`"bare"|"app"`, next-attach only), `tokensFrom?: "project"` (Tier 1 only), `network?: "off"|"allow"` (default off).

| Tool | Purpose | Key input | Returns | Annotations |
|---|---|---|---|---|
| **`stage_render`** | Create a stage and show it to the user | `title`, `StageSource`, options, `stageId?` (reuse → new rev), `snapshot?: boolean` (default false), `present?: "auto"\|"silent"` | `stageId`, `rev`, `adapter`, `status`, diagnostics summary, `url` (for the agent's own curl/debug), optional images | `readOnlyHint:false, destructiveHint:false, idempotentHint:false, openWorldHint:false` |
| **`stage_update`** | New revision of an existing stage. With no fields, **re-verifies after file edits** (waits for HMR) | `stageId`, any `StageSource`/option fields, `note?` (shown in the rev history) | Same as render | same |
| **`stage_compare`** | Side-by-side variants for the user to choose from | `title`, `question?`, `variants: [{label, description?, …StageSource+options}]` (2–4), or shorthand `file` + `exports: ["VariantA","VariantB","VariantC"]` | `compareId`, per-variant `stageId`+status | same |
| **`stage_snapshot`** | Visual QA: PNG(s) + layout/console diagnostics | `stageId`, `rev?`, `viewports?`, `theme?` (`both` → 2 images), `fullPage?`, `selector?` (element clip), `scenario?`, `detail?: "standard"\|"high"` | `image` content blocks + `structuredContent.diagnostics` + `resource_link`s to PNG files | `readOnlyHint:true, openWorldHint:false` |
| **`stage_inspect`** | Text-only element QA (for non-vision use or precision) | `stageId`, `selector` \| `pinId`, `viewport?` | DOM outline (depth-limited), `data-gantry-loc`, component name, curated computed styles, resolved CSS vars, box, a11y name/role | readOnly |
| **`stage_feedback`** | Read the user's pins/comments (and compare picks) | `stageId?` (default: all open in thread), `status?: "open"\|"all"` | `StagePin[]` (§6.6), crops as `resource_link` | readOnly |
| **`stage_list`** | What's open / recent | `scope?: "thread"\|"project"`, `limit?` | `[{stageId, title, rev, status, adapter, updatedAt}]` | readOnly |
| **`stage_setup`** | Detect + scaffold Tier 2 for a project/app | `app?`, `write?: boolean` (default true) | Analyzer report (§4.4), files written (`.gantry/stage.json`, `preview.tsx`, `gantry-env.d.ts`, `tsconfig.json`, info/exclude entries), **TODOs for the agent** (e.g. "UserProvider needs a mock user") | `destructiveHint:false` (only creates files under `.gantry/`) |
| **`stage_close`** | Close stage(s), clean generated routes | `stageId` \| `"all"` | ok | `destructiveHint:false, idempotentHint:true` |

**Names** follow the spec's `[A-Za-z0-9_.-]{1,128}` rule ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)). Claude sees them as `mcp__gantry__stage_render` ([Claude Code MCP](https://code.claude.com/docs/en/mcp)).

**`outputSchema` and `structuredContent`:** every tool declares an `outputSchema` (the JSON Schema of `StageResult`) and returns `structuredContent`, **plus** a human-readable `text` block, as the spec recommends ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)). The text block is what most agents actually read, so it's written for models:

```
✗ stage "pricing" rev 3 FAILED at runtime (vite-harness · apps/client-panel · 1.2s)
  TypeError: Cannot read properties of undefined (reading 'plan')
    at PricingCard (.gantry/stage/pricing/Pricing.tsx:42:18)
    in PricingCard ← PricingGrid ← Wrapper(scenario=default)
  console.error ×1: Warning: Each child in a list should have a unique "key" prop. (Pricing.tsx:61)
  network: GET /api/plans → blocked-mock (network is off; add data to scenario or props)
Fix the error, then call stage_update({stageId:"pricing"}) to re-verify. The user has NOT been shown this revision.
```

```
✓ stage "pricing" rev 4 ok (vite-harness · 840ms) · shown to user in Stage dock
  viewports: mobile 390×844, desktop 1440×900 · theme: light
  layout: ⚠ horizontal overflow at mobile: .pricing-table (+46px) at Pricing.tsx:88
  console: clean · network: clean
Tip: call stage_snapshot({stageId:"pricing", viewports:["mobile"]}) to visually check before telling the user it's done.
```

**`isError` policy:** `true` for build/resolve errors, uncaught runtime errors, empty render, and timeout-with-errors. `false` for warnings and layout issues, which are reported but don't block. Protocol errors (unknown stage, bad args) are JSON-RPC errors only for truly malformed requests. Everything else is `isError` text, so the model can self-correct ([tools: error handling](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).

**Presenting to the user:** `present: "auto"` means the dock auto-focuses only on the first *ok* rev of a stage in the current turn. Failing revs never steal focus. They show in the timeline as "draft · failing". `"silent"` is for agent-internal checks.

### 6.3 `stage_compare` UX

- The dock switches to **compare layout**: 2–4 synchronized frames (same viewport/theme/scenario/scroll), each labeled A/B/C with the agent's one-line description, plus the `question` banner ("Which nav direction?").
- The user can **pick**, **pick + comment**, **mix** ("A's layout with C's colors"), or **reject all + comment**. The result is a structured user message posted to the thread (and readable via `stage_feedback`):

```xml
<stage_compare_result compare="nav-variants" picked="B">
  <comment>B, but use A's logo spacing</comment>
</stage_compare_result>
```

- One file with multiple named exports (`VariantA/B/C`) is the cheapest agent pattern. The skill recommends it.

### 6.4 Snapshot image budget

- Claude downsizes images above ~1568 px long edge / ~1.15 MP (≈1,600 tokens). Newer high-res models (Opus 4.7+) accept up to 2576 px. Image cost ≈ `w·h/750` tokens ([Claude vision](https://platform.claude.com/docs/en/build-with-claude/vision)).
- **Default:** capture at DPR 1, downscale so the long edge is ≤ 1568 px and area ≤ 1.15 MP, and encode as PNG. Use **JPEG q85 for full-page captures over 1 MP** (much smaller payload; MCP image content takes any `mimeType`). `detail: "high"` raises the cap to 2576 px.
- **Tall pages:** `fullPage` caps at 4× viewport height. Beyond that, return the first tile plus a note ("page is 6200px tall; use selector to focus").
- **Codex** forwards MCP images as `InputImage` when the model supports images ([mcp_tool_call.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/mcp_tool_call.rs)). **Claude Code** shows PNG/JPEG/GIF/WebP inline ([Claude Code MCP](https://code.claude.com/docs/en/mcp)). **Antigravity:** image passthrough is unverified, so every snapshot also returns a `resource_link` and writes PNGs to `<appRoot>/.gantry/stage/<id>/snapshots/` (inside the workspace, so the agent's own file/image-view tools can open them) ⚠ Spike.
- **Auto thumbnails** for the timeline are always taken (cheap, 640 px wide, not sent to the model).

### 6.5 Wiring all three agents (MCP + Skill + prompt)

Three layers, from always-on to on-demand:

1. **MCP server `instructions`** (sent in the initialize/discover result; ~80 tokens): *"Gantry Design Stage: render UI so the user can see it. Use stage_render for any UI you create or change; verify with stage_snapshot before claiming visual results; read stage_feedback when the user mentions pins. Full guide: skill `gantry-design-stage` or resource gantry://guide."* ⚠ Spike: confirm each agent surfaces server instructions.
2. **Prompt addendum** (~120 tokens), injected via each provider's system/developer channel:
   - Claude: `--append-system-prompt-file` ([CLI ref](https://code.claude.com/docs/en/cli-reference)).
   - Codex: `thread/start.developerInstructions`. This field is present in Codex's local `ThreadStartParams` schema (`docs/research/schemas/codex/v2/ThreadStartParams.json`).
   - Antigravity: no system-prompt flag exists ([agy CLI ref](https://antigravity.google/docs/cli/reference)), so it's prepended as a `<gantry_context>` block on the first prompt of a session.
3. **Agent Skill `gantry-design-stage`** in the open SKILL.md format: `name` + `description` frontmatter, progressive disclosure, body < 500 lines ([agentskills.io spec](https://agentskills.io/specification)).

| | Claude Code | Codex (`app-server`) | Antigravity (`agy acp`) |
|---|---|---|---|
| **MCP** | `--mcp-config '{"mcpServers":{"gantry":{"type":"http","url":"http://127.0.0.1:P1/mcp","headers":{"Authorization":"Bearer <tok>"}}}}'`. **Not** `--strict-mcp-config`, so the user's own servers keep working ([CLI ref](https://code.claude.com/docs/en/cli-reference), [MCP](https://code.claude.com/docs/en/mcp)) | `thread/start.config`: `mcp_servers.gantry.url`, `mcp_servers.gantry.http_headers` (`Authorization`), `tool_timeout_sec=180`, `default_tools_approval_mode="auto"` ([config reference](https://learn.chatgpt.com/docs/config-file/config-reference)) ⚠ Spike: dotted keys in `config` map | `session/new.mcpServers: [{type:"http", name:"gantry", url, headers:[{name:"Authorization",value:"Bearer <tok>"}]}]` ([ACP session setup](https://agentclientprotocol.com/protocol/session-setup)). Run `agy acp --process-per-session` (required for client-configured HTTP MCP per [agy ACP docs](https://antigravity.google/docs/cli/acp.md)) ⚠ Spike |
| **Auto-approve stage tools** | `--allowedTools mcp__gantry` (server-level rule) | `default_tools_approval_mode = "auto"` on the gantry server | permission pattern `mcp(gantry/*)` ([agy MCP](https://antigravity.google/docs/mcp)) |
| **Skill install (without touching the user's repo)** | `--plugin-dir <appdata>/gantry/agent-pack/claude-plugin` (plugin containing `skills/gantry-design-stage/`), or `--add-dir` of a dir containing `.claude/skills/` ([skills](https://code.claude.com/docs/en/skills)) | `skills/extraRoots/set { extraRoots: ["<appdata>/gantry/agent-pack/skills"] }`. Present in the local Codex v2 schema (`SkillsExtraRootsSetParams.json`) | No extra-roots API documented. Install to `~/.gemini/config/skills/gantry-design-stage/` on first run **with consent** (Q5). The description self-gates ("…only when gantry stage tools are available") ([agy skills](https://antigravity.google/docs/skills)) |
| **Images to model** | ✅ | ✅ (model-dependent) | ⚠ unverified → `resource_link` + workspace PNG |
| **Pins as user-message images** | stream-json user content blocks | `turn/start.input` image items | ACP prompt image content (`promptCapabilities.image`) ([ACP init](https://agentclientprotocol.com/protocol/initialization)) |

The cross-vendor `.agents/skills/` folder (Codex + Antigravity) is **not** used, because it would write into the owner's repos. Gantry keeps one canonical skill source in its resources and materializes it into the three locations above.

**Future-proofing:** also expose the skill via the MCP **Skills extension** (`io.modelcontextprotocol/skills`, `skills/list`, `skill://…`) once hosts implement it ([skills extension](https://modelcontextprotocol.io/extensions/skills/overview)). Also expose it as an MCP resource `gantry://guide`, so any agent can `resources/read` it.

#### The prompt addendum (verbatim draft)

```text
You are running inside Gantry, a desktop workbench with a live Design Stage the user is looking at.
When you create or change anything visual (pages, components, layouts, styles), SHOW it: render it
with the gantry stage_* tools instead of describing it. Prefer the user's real components (Tier 2:
write a mockup under .gantry/stage/<id>/ and call stage_render with `file`); use inline `source`
only for quick standalone sketches. Fix any errors stage tools report before telling the user to look,
and use stage_snapshot to check your work visually. Load skill `gantry-design-stage` for details.
```

#### The Skill (`gantry-design-stage/SKILL.md`, draft)

```markdown
---
name: gantry-design-stage
description: >
  Render live UI mockups and real project components in Gantry's Design Stage so the user can see
  designs, compare variants, and leave pinned feedback. Use whenever you design, build, restyle or
  review UI (pages, components, layouts, themes, responsive behavior), when the user asks to "show",
  "mock up", "preview", "try variants", or mentions pins/feedback on the stage. Only applies when
  the gantry stage_* MCP tools are available.
---

# Gantry Design Stage

## Pick the tier
- **Real project UI (default when working in a project):** write a mockup module under
  `.gantry/stage/<kebab-id>/` that imports the project's real components, tokens and styles, then
  `stage_render({ title, file: ".gantry/stage/<id>/<Name>.tsx" })`. Iterate by editing the file with
  your normal edit tools and calling `stage_update({ stageId })` to re-verify (HMR updates the user's view).
- **Existing route/page:** `stage_render({ title, url: "/settings" })`.
- **Quick standalone sketch / no project / exploring ideas:** `stage_render({ title, source: { kind: "tsx",
  files: { "App.tsx": "..." } } })`. Tailwind v4 utilities work out of the box. Allowed imports: react,
  react-dom, lucide-react, clsx, tailwind-merge, class-variance-authority, motion, recharts, date-fns, zod.
  No network.

## First time in a project
If a Tier 2 render reports `setup_required`, call `stage_setup()`. Then open `.gantry/preview.tsx` and
finish its TODOs: import global CSS/fonts, wrap with the app's client providers, and replace server-fetched
data with `scenarios` (named mock data sets). Never import server-only modules into the wrapper. See
references/wrappers.md for Next.js, Vite, TanStack Router/Start, react-router, Tailwind v3/v4, and SCSS recipes.

## Rules
1. Read and obey the project's DESIGN.md / AGENTS.md / CLAUDE.md. Compose the project's UI kit; don't
   hand-roll styles the kit already provides; use tokens, not raw values.
2. Mockups are throwaway files in `.gantry/stage/` (git-excluded). Never put mockups in src/ unless the user
   asks to promote one. Promotion = move into the codebase following conventions + run the project's checks.
3. Never tell the user a render is ready while the stage tool reports errors. Fix them first: errors include
   file:line in `.gantry/stage/...` or the component that threw.
4. Before saying a design is done, call `stage_snapshot` for the relevant viewports (always include
   mobile for responsive work, both themes if the app supports dark mode) and actually look. Check
   `layout.horizontalOverflow` and `clippedText`.
5. Offering options? Use `stage_compare` (one file exporting VariantA/B/C works well), ask one clear question,
   then wait for the user's pick.
6. When the user's message contains `<stage_feedback>` pins, address each pin (the `loc` tells you which line
   to edit), re-verify with `stage_update`, and summarize what changed per pin.
7. Don't start your own dev servers or browsers for previews; the stage tools manage them.
8. Keep mock data realistic (real-looking names, numbers, long strings) to surface layout issues.

## References
- references/wrappers.md: per-framework preview wrapper recipes
- references/diagnostics.md: what each diagnostic means and typical fixes
- references/patterns.md: page mockup skeletons, responsive checklists, empty/loading/error states
```

### 6.6 Interactive user → agent design feedback loop

The loop: **agent renders → user looks, toggles viewport/theme, pins comments on elements → the pins ride along with the next chat message as structured context plus cropped images → agent edits files → HMR updates the stage live → agent snapshots to verify → user sees the new rev.**

#### Inspector mode (`⌖` toggle or hold `Alt`)

The Stage runtime (injected into every stage document, both tiers) implements:

- **Hover:** outline the element under the cursor, with a label `Header › nav.links › a` plus the component name when known.
- **Click:** freeze the selection and open a comment popover anchored *in the host UI* (not inside the iframe, so it's styled by Gantry and survives iframe reloads). Multi-select with `Shift`. Area-drag selects everything intersecting a rectangle.
- **Pin descriptor** (computed inside the iframe, posted to host):

```ts
interface StagePin {
  pinId: string; stageId: string; rev: number; viewport: string; theme: 'light' | 'dark';
  selector: string;                 // robust CSS path: prefers [data-testid], [id], then nth-of-type chain
  loc?: string;                     // from data-gantry-loc: ".gantry/stage/pricing/Mockup.tsx:42:9"
  ownerLoc?: string;                // nearest ancestor whose loc is in a *different* file (call site of the component)
  component?: string;               // nearest React component name (fiber walk, dev only) e.g. "PricingCard"
  ownerStack?: string;              // React captureOwnerStack() when available (dev only, React ≥19.1)
  tag: string; text: string;        // first 120 chars of innerText
  classes: string[];                // className tokens (Tailwind utilities / CSS-module hashes)
  box: { x: number; y: number; w: number; h: number };            // CSS px in the stage viewport
  styles: Record<string, string>;   // curated computed subset: font-*, line-height, color, background-color,
                                    // padding, margin, gap, border-radius, box-shadow, display, flex/grid props, width/height
  vars?: Record<string, string>;    // custom properties that resolved into the above (e.g. --muted-foreground)
  comment: string;                  // what the user typed
  cropPng?: string;                 // element crop (with 16px margin) captured by host via snapshot service
}
```

How `loc` works: React 19 removed the `_debugSource` / `__source` dev metadata that click-to-source tools used to read (see §4.8). So Gantry adds its own. The oxc transform (Tier 1) and the Gantry Vite plugin's transform (Tier 2, **only** for files under `.gantry/stage/**` and optionally the project's `src/**`) add `data-gantry-loc="relpath:line:col"` to **host** JSX elements. This is the same approach as `code-inspector-plugin` and LocatorJS. It's dev-only and never on disk. For components from a UI kit, `ownerLoc` points at the **mockup line that used the component**, which is the line the agent should edit 90% of the time.

#### Composer integration

- Pins appear as chips above the composer: `📌 Header › nav (Mockup.tsx:42) "make this tighter, use muted foreground"`.
- On send, Gantry serializes them into the user message as a compact, model-friendly block. The crops are attached as images where the provider supports user-message images (Claude stream-json: yes; Codex `turn/start` input items: `localImage`; ACP: image content blocks, verified per provider by Neon #2):

```xml
<stage_feedback stage="pricing" rev="4" viewport="mobile-390" theme="dark">
  <pin id="p1" loc=".gantry/stage/pricing/Mockup.tsx:42:9" component="PricingCard"
       selector="section.pricing > div:nth-of-type(2) h3">
    <comment>make this header tighter, use muted foreground</comment>
    <computed font-size="20px" line-height="32px" color="var(--foreground) → #ededed" margin-bottom="24px"/>
  </pin>
</stage_feedback>
```

- The agent's `stage_*` tools can also read pins (`stage_feedback(stageId)`) in case the agent wants the full descriptor or crop later in the turn.
- **Resolution:** when the agent's next `stage_render`/HMR rev lands, Gantry re-queries each pin's selector in the new rev. If the element still exists, the pin shows "↻ revised in rev 5". The user can mark ✓ resolved or keep it open. Unresolved pins persist (SQLite) per stage.

#### Other loop accelerators

- **"Variant from here"**: right-click a pin → "Ask for 3 variants of this element". This sends a templated message that makes the agent call `stage_compare`.
- **Live tweak (phase 2):** for pinned elements, expose a tiny style panel (spacing / color token / font-size steps from the project's tokens). Edits are applied as an *ephemeral* inline-style overlay in the iframe and recorded as a **diff of intent** (`padding-top 24px → 16px (--space-4)`) attached to the next message. Gantry never writes source itself. The agent translates intent into the house style, e.g. pockethost's "token names only" rule (`packages/ui` DESIGN.md forbids raw px in pages).
- **Scroll/route sync:** the stage reports `scrollY` and the current route (Tier 2 `app` frame). Pins capture them so the agent can reproduce the exact view in `stage_snapshot`.


### 6.7 MCP Apps: compatible, not dependent

MCP Apps (`io.modelcontextprotocol/ui`, `ui://` resources rendered in sandboxed iframes, a JSON-RPC postMessage bridge) became the first official extension on 2026-01-26. It's supported by Claude (web/desktop), VS Code Copilot, Goose and others ([announcement](https://blog.modelcontextprotocol.io/posts/2026-01-26-mcp-apps/), [spec](https://raw.githubusercontent.com/modelcontextprotocol/ext-apps/main/specification/2026-01-26/apps.mdx), [overview](https://modelcontextprotocol.io/extensions/apps/overview)). Codex app-server passes `mcpAppUi`/`mcpAppResourceUri` through to its client ([app-server README](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)). Claude Code **hides** `ui://` resources and only gives the model tools whose `_meta.ui.visibility` includes `"model"` ([Claude Code MCP](https://code.claude.com/docs/en/mcp)).

Decisions:

1. **Gantry doesn't need MCP Apps for its own stage.** It owns both the MCP server and the host UI, so tool calls update the dock directly through `StageService`. That's lower latency and richer (HMR, Tier 2 origins, pins) than the MCP Apps bridge.
2. **Still declare** `_meta.ui.resourceUri: "ui://gantry/stage"` and `visibility: ["model","app"]` on the stage tools, and serve a small `text/html;profile=mcp-app` viewer. It shows the snapshot plus a "open in Gantry" deep link. This costs ~1 day and makes the stage tools degrade gracefully if the owner points another MCP-Apps host at Gantry's server.
3. **Phase 3:** make Gantry an **MCP Apps host for third-party servers** that agents use (Codex passes the metadata through). Render their `ui://` views in the same sandboxed-iframe infrastructure (separate origin, the spec's sandbox-proxy requirement).

---

## 7. Frontend app architecture (React 19 + TypeScript)

### 7.1 Stack (opinionated)

| Concern | Choice | Why |
|---|---|---|
| Build | Vite 8 + `@vitejs/plugin-react` | Same toolchain the owner already uses (Vite 8.3 in `vite-hono-base`, `airfieldv2`). Tauri's own Vite template path. |
| Language | TypeScript strict, `verbatimModuleSyntax` | Matches house style. |
| Routing | None (or a 30-line hash router) | It's a single-window workbench. Views are panes, not pages. Avoid router-driven remounts of live iframes. |
| Server/event state | **Hand-written external store** (`ThreadLog`) + `useSyncExternalStore` | 500+ tok/s needs per-item subscriptions and rAF coalescing. Zustand isn't wrong, but the hot path needs control Zustand's single-store API doesn't give you. |
| UI state | Zustand (layout, dock, selection, composer drafts, settings) | Low-frequency, ergonomic, selector-based ([Zustand transient updates](https://deepwiki.com/pmndrs/zustand/7.4-transient-updates)). |
| Timeline virtualization | `@tanstack/react-virtual` with the chat recipe (`anchorTo: 'end'`, `measureElement`) | Official chat/reverse-feed guide ([TanStack Virtual chat docs](https://tanstack.com/virtual/latest/docs/chat)); `virtua` is the fallback ([virtua README](https://raw.githubusercontent.com/inokawa/virtua/main/README.md)). |
| Streaming markdown | `streamdown` (+ `remend`) for the *live* item only; completed items render once and freeze | Handles unterminated fences/emphasis while streaming ([streamdown.ai docs](https://streamdown.ai/docs), [termination](https://streamdown.ai/docs/termination), [Vercel changelog](https://vercel.com/changelog/introducing-streamdown)). |
| Syntax highlight | Shiki in a Web Worker, highlight on block completion only | Never highlight a half-streamed fence on the main thread. |
| Styling | CSS Modules + tokens CSS (or Tailwind v4) — Gantry's own choice, **class-based theme (`data-theme`)**, never `prefers-color-scheme` in Gantry's own CSS | Keeps Gantry's chrome immune to any color-scheme emulation we apply to stage frames (§5.6). |
| Diff | `@pierre/diffs` / `react-diff-view` / Monaco diff (decide in spike) | Diff drawer only; lazy-loaded. |
| Terminal (later) | xterm.js | Lazy-loaded. |

### 7.2 Layout

```
┌──────────────┬───────────────────────────────────────────┬──────────────────────────────┐
│ PROJECTS     │  Thread header: agent ▾  model ▾  branch  │  DOCK  [Stage][Diff][Files]  │
│ ▾ gantry     │  ───────────────────────────────────────  │ ┌──────────────────────────┐ │
│   ● main     │  user ▸ "make the pricing page tighter"   │ │ Stage: Pricing v3  ▾ rev │ │
│   ○ wt/stage │  assistant ▸ markdown…                    │ │ [📱][▭][🖥][⛶] [☀/☾] [⌖] │ │
│ ▾ pockethost │  ▸ tool: Edit src/…  (collapsed card)     │ │ ┌──────────────────────┐ │ │
│   ● main     │  ▸ 🎨 stage_render "Pricing v3" [thumb]   │ │ │   live iframe         │ │ │
│              │     ✓ 0 errors · 390/768/1440 · open ▸    │ │ │                       │ │ │
│ STAGES       │  ▸ permission: run `bun test`? [Allow]    │ │ └──────────────────────┘ │ │
│  Pricing v3  │  ▸ question: "Which nav?" [A][B][C]       │ │ console (2) · pins (1)   │ │
│  Nav A/B/C   │  ───────────────────────────────────────  │ └──────────────────────────┘ │
│              │  [📌 Header pin] [composer…………………] ⏎    │                              │
└──────────────┴───────────────────────────────────────────┴──────────────────────────────┘
```

- **Left rail:** Projects → worktree threads (status dot: idle / streaming / waiting-on-you / error), plus a per-project **Stages** list (recent stage sessions, pinned compares).
- **Center:** virtualized timeline + composer. The composer shows **pin chips** (stage feedback) as structured attachments.
- **Right dock:** tabs for **Stage**, **Diff**, **Files**, later **Terminal**. Resizable, collapsible, and the Stage tab can **pop out** into its own Tauri window (multi-monitor design review is a real workflow). Popping out re-mounts the iframe at the same URL. Tier 2 state survives because it's HMR-backed. Tier 1 state is just the document.
- **Focus mode:** `Ctrl+Shift+S` makes the Stage full-window with the composer floating. This is the "show me the design" mode.

### 7.3 The event pipeline (Rust → React) that survives 500+ tok/s

The rule: **React never sees a token. React sees a frame.**

```
Rust (per-thread Channel<ThreadEventBatch>)          JS main thread
────────────────────────────────────────────          ──────────────────────────────────────────
provider adapters → normalized ThreadEvent  ──IPC──►  channel.onmessage(batch)
  coalesced every ≤16ms or ≤64 events                  └► ThreadLog.apply(batch)      // plain TS, no React
  monotonically increasing `seq`                           • items: Map<itemId, ItemState> (append-only order[])
                                                           • text deltas appended to a rope/chunk array
                                                           • marks itemId dirty
                                                        requestAnimationFrame(flush)
                                                           └► notify subscribers of dirty itemIds only
React
  <Timeline> subscribes to `order.length` (+ status)  → re-renders only when items are added
  <Item id> subscribes to `items.get(id).version`      → only the streaming item re-renders, ≤60×/s
```

Concrete rules:

1. **Rust batches.** Neon #2's provider adapters emit into a per-thread `tauri::ipc::Channel`. The Rust side coalesces deltas (≤16 ms or ≤64 events per message). This turns 500 msgs/s into ≤60 IPC messages/s. Tauri documents Channels as the ordered, fast streaming primitive, vs. events, which go through JSON eval ([Tauri: Calling the frontend from Rust](https://v2.tauri.app/develop/calling-frontend/)).
2. **`seq` everywhere.** Every batch carries `{threadId, fromSeq, toSeq}`. On a gap (webview reload, hot reload of Gantry itself), the store calls `thread_replay(threadId, sinceSeq)`, which Rust answers from SQLite. This makes the UI a pure projection of the durable log. Reloading the webview is free.
3. **The external store is not React state.** `ThreadLog` is a mutable class with `subscribe(itemId, cb)` and `getSnapshot(itemId)` returning an immutable per-item snapshot object that is replaced only on flush. Components use `useSyncExternalStore(subscribe, getSnapshot)` ([react.dev useSyncExternalStore](https://react.dev/reference/react/useSyncExternalStore)). Text is stored as `chunks: string[]`. The snapshot exposes `text` lazily joined once per frame.
4. **Freeze completed items.** On `item.completed`, the item's markdown is parsed once into a memoized tree (Streamdown static mode, or `react-markdown` with the same plugins), code blocks are sent to the Shiki worker, and the component becomes `React.memo` with a never-changing snapshot. Only one item per thread is "hot".
5. **Thinking / tool output** streams into collapsed cards by default. Collapsed cards don't render their body, so a 50 KB/s `bun test` stream costs nothing until expanded. Expanded tool output uses a plain `<pre>` with a capped tail (last N KB) plus "open full log".
6. **Virtualization** uses `@tanstack/react-virtual` with dynamic measurement and end-anchoring, so the hot item growing at the bottom doesn't fight the scroll position ([TanStack Virtual chat guide](https://tanstack.com/virtual/latest/docs/chat)). Auto-follow only when the user is within ~80 px of the bottom.
7. **Transitions for heavy non-stream work only:** thread switch and diff open use `startTransition`. Token flow uses rAF batching, not transitions.
8. **Never put a live iframe in the virtualized list.** Stage cards in the timeline show a **PNG thumbnail** (from the auto-snapshot) plus status chips. Clicking opens the live stage in the dock. Virtualization unmounts rows. An unmounted iframe means a reloaded document, lost HMR state, and N renderers' worth of RAM. The dock keeps at most **3 live stage iframes** (LRU). Others park as thumbnails.

### 7.4 Interactive prompts

- **Permission prompts** (Claude `can_use_tool` / Codex approval requests / ACP `session/request_permission`, all normalized by Neon #2) render as **sticky cards pinned above the composer**, not just timeline items. They block the thread, so they must be impossible to miss. Keyboard: `A` allow once, `Shift+A` allow for session, `D` deny with optional note.
- **User questions** (Claude `AskUserQuestion`, Codex `request_user_input`, Gantry's own `stage_compare` picks) render as option chips. Each option can have a preview (the stage compare thumbnail).
- **Stage compare picks** produce a structured user message: `Chose variant B ("Dense") for "Nav" — note: "but keep A's logo spacing"`.

### 7.5 Store shape (TS)

```ts
type ThreadId = string; type ItemId = string; type StageId = string;

interface ThreadEventBatch { threadId: ThreadId; fromSeq: number; toSeq: number; events: ThreadEvent[] }

type ThreadEvent =
  | { t: 'item.started'; item: ItemHeader }                  // message | thinking | tool_call | stage | permission | question | error
  | { t: 'item.delta'; id: ItemId; field: 'text' | 'thinking' | 'output'; chunk: string }
  | { t: 'item.patch'; id: ItemId; patch: Partial<ItemMeta> } // tool status, exit code, stage status…
  | { t: 'item.completed'; id: ItemId; final?: ItemMeta }
  | { t: 'turn.status'; status: 'running' | 'waiting_permission' | 'waiting_user' | 'idle' | 'error' }
  | { t: 'usage'; inputTokens: number; outputTokens: number; costUsd?: number };

// Stage events go on a separate per-window Channel (they aren't per-thread hot path)
type StageEvent =
  | { t: 'stage.created'; stage: StageSummary }
  | { t: 'stage.revision'; stageId: StageId; rev: number; url: string; status: 'building' | 'ready' | 'error' }
  | { t: 'stage.diagnostics'; stageId: StageId; rev: number; diagnostics: StageDiagnostics }
  | { t: 'stage.snapshot'; stageId: StageId; rev: number; viewport: string; theme: 'light' | 'dark'; pngPath: string }
  | { t: 'runner.status'; projectId: string; adapter: AdapterId; status: 'starting' | 'ready' | 'crashed'; port?: number; log?: string };
```

---

## 8. Interface contract with the Rust backend (for Neon #2)

### 8.1 What I need Rust to own

| Module | Responsibility |
|---|---|
| `mcp::server` | Streamable HTTP endpoint (2025-11-25 + 2026-07-28), bearer→thread identity, Origin rejection, progress notifications, `outputSchema`/`structuredContent`, image content. Tool handlers are thin and call `StageService`. |
| `stage::service` | Stage registry, revisions, adapter selection, orchestration of compile → serve → verify → persist → emit. The single source of truth the UI and MCP both read. |
| `stage::tier1` | oxc transform (TS strip, JSX automatic, `data-gantry-loc` pass), specifier rewrite, stdlib allowlist, immutable rev dirs. |
| `stage::host` | axum on `127.0.0.1:P2`. Serves Tier 1 revs, runtime, stdlib, `ACAO:*` on Tier 1 assets, Host allowlist, path tokens. |
| `stage::runner` | Spawn/supervise one Node Stage Runner per app root (Job Object, idle-kill 10 min, crash → restart with backoff, stderr ring buffer). JSONL protocol (§8.3). Node/bun discovery. |
| `stage::next_attach` | `.next/dev/lock` reader, route codegen/cleanup with hash-guarded deletes, dev-server ownership via the process supervisor. |
| `stage::verify` | Headless Edge discovery/launch (`--remote-debugging-pipe`), CDP session pool, settle protocol, diagnostics collection, snapshot capture + downscale (`image` crate). |
| `stage::git` | `git rev-parse --git-path info/exclude` + idempotent append. Never touches `.gitignore`. |
| `stage::analyze` | Project analyzer (§4.4), `stage_setup` scaffolding (templates shipped in resources). |
| `fonts::cache` | Offline cache for Google Fonts fetched by `next/font/google` shims (served to runners via a local URL). |

### 8.2 Tauri commands and channels the frontend expects

```rust
// Commands (all async)
#[tauri::command] async fn thread_subscribe(thread_id: String, since_seq: u64, ch: Channel<ThreadEventBatch>) -> Result<(), E>;
#[tauri::command] async fn thread_replay(thread_id: String, since_seq: u64) -> Result<Vec<ThreadEventBatch>, E>;
#[tauri::command] async fn stage_subscribe(ch: Channel<StageEvent>) -> Result<(), E>;
#[tauri::command] async fn stage_list(scope: StageScope) -> Result<Vec<StageSummary>, E>;
#[tauri::command] async fn stage_get(stage_id: String) -> Result<StageDetail, E>;          // revs, diagnostics, snapshots, pins
#[tauri::command] async fn stage_frame(stage_id: String, rev: Option<u32>) -> Result<StageFrame, E>; // { url, sandbox, allowSameOrigin }
#[tauri::command] async fn stage_pin_upsert(pin: StagePinInput) -> Result<StagePin, E>;
#[tauri::command] async fn stage_pin_crop(stage_id: String, rect: Rect, dpr: f32) -> Result<PathBuf, E>; // WebView2 CDP clip of the visible dock
#[tauri::command] async fn stage_compare_pick(compare_id: String, pick: ComparePick) -> Result<(), E>;
#[tauri::command] async fn stage_close(stage_id: String) -> Result<(), E>;
#[tauri::command] async fn project_stage_status(project_id: String) -> Result<ProjectStageStatus, E>; // adapter, runner state, setup TODOs
#[tauri::command] async fn runner_restart(app_root: String) -> Result<(), E>;
#[tauri::command] async fn composer_send(thread_id: String, msg: ComposerMessage) -> Result<(), E>;  // text + pins + images
```

- **Images to the UI:** pass **file paths**, rendered via Tauri's asset protocol (`convertFileSrc`), not bytes over IPC. If bytes are ever needed, use `InvokeResponseBody::Raw` → `ArrayBuffer`, since `Vec<u8>` serializes as a JSON number array ([channel.rs](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/ipc/channel.rs)).
- **Batching:** `ThreadEventBatch` coalesced ≤16 ms / ≤64 events, with monotonically increasing `seq` per thread, durable in SQLite before emit, so replay is exact. Channels are ordered. Events are not, and shouldn't carry the stream ([calling the frontend](https://v2.tauri.app/develop/calling-frontend/)).
- **Pin crops** need `webview.with_webview(|pw| pw.controller()…CoreWebView2()…CallDevToolsProtocolMethod("Page.captureScreenshot", {clip}))` on the main window ([webview/mod.rs](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/webview/mod.rs)).

### 8.3 Stage Runner JSONL protocol (Rust ⇄ Node, over stdio)

```jsonc
// Rust → runner
{"id":1,"op":"start","appRoot":"C:/…/apps/client-panel","adapter":"next-vite","config":{…stage.json app…},"token":"…","fontCacheUrl":"http://127.0.0.1:P2/fonts"}
{"id":2,"op":"addStage","stage":{"id":"pricing","file":".gantry/stage/pricing/Pricing.tsx","export":"default","props":{},"scenario":"default"}}
{"id":3,"op":"updateStage","stage":{…}}   {"id":4,"op":"removeStage","stageId":"pricing"}   {"id":5,"op":"stop"}
// runner → Rust
{"re":1,"ok":true,"origin":"http://127.0.0.1:52133","vite":"8.3.2","react":"19.3.1"}
{"re":2,"ok":true,"url":"http://127.0.0.1:52133/__gantry/stage/pricing?t=…"}
{"ev":"progress","message":"optimizing dependencies (radix-ui, lucide-react)…"}
{"ev":"build-error","stageId":"pricing","error":{"file":".gantry/stage/pricing/Pricing.tsx","line":12,"col":5,"message":"…","plugin":"vite:oxc"}}
{"ev":"hmr","stageId":"pricing","files":[".gantry/stage/pricing/Pricing.tsx"]}   // lets VerifyEngine know to re-settle
{"ev":"log","level":"warn","message":"…"}
```

### 8.4 Questions for Neon #2

1. **One `codex app-server` process per Gantry thread, or shared?** This decides whether the MCP bearer token can be per-process env (`bearer_token_env_var`) or must be in `thread/start.config` `http_headers`.
2. **MCP library:** does `rmcp` (or your choice) let us serve **both** 2025-11-25 and 2026-07-28 on one endpoint, including `outputSchema`, image content, and progress on the response stream? If not, we hand-roll on axum (≈1.5k LOC).
3. **ACP / `agy`:** can you confirm `session/new.mcpServers` with `type:"http"` is honored (and whether `--process-per-session` is required)? Do ACP tool results with images reach the model?
4. **Job Objects:** put Stage Runners, Gantry-owned `next dev`, and headless Edge in the same per-project Job (kill-on-close)? Edge spawns its own process tree. Confirm `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` catches its children when launched with `--remote-debugging-pipe`.
5. **User-message attachments:** what's the normalized `ComposerMessage` → provider mapping for images (Claude stream-json image blocks, Codex `localImage`, ACP image content)? Pins depend on it.
6. **Minimum WebView2 runtime:** I need ≥ 1.0.2365.46 (custom-protocol iframe fix) and `ICoreWebView2_11` (CDP for sessions). Will you enforce a runtime version check at startup? Also, track wry PR #1874 (IPC scripts injected into all frames).
7. **SQLite schema ownership:** I'll propose `stages`, `stage_revisions`, `stage_diagnostics`, `stage_snapshots`, `stage_pins`, `compare_groups`. Do you want them in the main DB or a separate `stage.db` (WAL) to keep thread-log writes hot?
8. **Thread event normalization:** please include `item.kind = "stage"` items when a `stage_*` tool call completes (with `stageId`, `rev`, `status`, `thumbPath`), so the timeline can render stage cards without joining across channels.

---

## 9. Questions for the owner (high-leverage, before code)

1. **Commit `.gantry/` config to your repos?** (`stage.json`, `preview.tsx`, `mocks/`, like `.storybook/`.) Recommended: **yes**. It's reusable across agents and machines. `.gantry/stage/` (the mockups) always stays git-excluded.
2. **Next.js dev server ownership:** Next 16 allows one `next dev` per project. OK for Gantry to **start/own** it (you'd use the same server in your browser), and to write **temporary, git-excluded** route files under `app/(gantry)/gantry-stage/`? If an auth `proxy.ts` blocks those routes, can the agent add a dev-only matcher exclusion?
3. **Gold-standard first target:** which project should Tier 2 be proven on first? My suggestion: **`pockethost-panel`** (Next 16 + workspace kit + `next/font` + async root layout + strict DESIGN.md is the hardest real case), then **`hono-trpc-tanstack-base`** (Vite 8 + TanStack Start + SCSS), then **`vite-hono-base`** (Tailwind v4 + media dark mode).
4. **Network in mockups:** default **mocked/offline** (scenarios + props), or allow Tier 2 mockups to hit your local Hono backends (`:6060`) through the project's proxy?
5. **Antigravity skill install:** OK to install the skill into `~/.gemini/config/skills/` (global for agy) on first run, since agy has no per-session skill path?
6. **Auto-approve** `stage_*` tools for all agents? Recommended yes. They only write under `.gantry/` and run local renders.
7. **Vision budget:** auto-attach a screenshot to *every* render (better self-QA, costs ~1–3k tokens per call), or only when the agent calls `stage_snapshot` (recommended, enforced via skill rule #4)?
8. **Non-React projects** (SolidStart): is `url`-mode (frame the real dev server + QA) enough, or do you want a Solid harness too?
9. **Cross-platform:** is Windows-only acceptable for v1 of the verify engine (system Edge), with Chrome / downloaded `chrome-headless-shell` as later fallbacks?
10. **Pins → messages:** include element crop images by default (better grounding, more tokens), or text descriptors only?
11. **Promote flow:** agent-led (recommended) vs. a mechanical "copy file to path" button?
12. **Stage retention:** keep mockups/snapshots 14 days by default? Any desire to export a stage (static HTML/PNG bundle) to share?

---

## 10. Hard-stuff-first build order and spikes

**Week 0–1: spikes that can kill or reshape the design** (each produces a runnable prototype + a findings note):

| Spike | Question | Pass criteria |
|---|---|---|
| S1 Tauri iframe security | Do cross-port `127.0.0.1` iframes (both sandbox profiles) render in Tauri v2 WebView2? Does `__TAURI_INTERNALS__` appear in them, and are invokes rejected? Does `color-scheme` propagation work there? | No IPC success from any stage frame. Theme flips per frame. |
| S2 VerifyEngine in Rust | `chromiumoxide` (or a raw client) + system Edge via `--remote-debugging-pipe`. Settle protocol, diagnostics, downscale. | p50 warm snapshot < 300 ms. Diagnostics match intentional errors. |
| S3 `next-vite` on **pockethost-panel** | Kit with `next/font/google` + Radix + `UserProvider` scenario, React from `next/dist/compiled`, offline font cache | Pixel-comparable to the real app page for 3 kit components. Cold start < 5 s. |
| S4 `vite-harness` on **hono-trpc-tanstack-base** + **vite-hono-base** | Plugin filtering (TanStack Start), SCSS loadPaths, `@styles` alias, Tailwind v4 `@source` injection, nested tsconfig paths from `.gantry/` | Mockup importing `@components/*` + SCSS modules renders. No files written outside `.gantry/`. |
| S5 `next-attach` on **starfall-starter** | Lockfile discovery, route codegen, HMR on mockup edits, `allowedDevOrigins`, cleanup | Async server component mockup renders in the real root layout. `git status` is clean throughout. |
| S6 Agent loop | Claude/Codex/agy each: MCP over HTTP with bearer, skill discovery, image results, `isError` self-heal | Each agent, given "mock up a pricing page with our kit", produces an ok render with ≤2 self-heal cycles. |

**Then:**

- **Phase 1:** StageService + MCP tools + Tier 1 + VerifyEngine + dock UI. The full loop works for standalone mockups.
- **Phase 2:** `vite-harness` + `next-vite` + `stage_setup` + pins/inspector + compare.
- **Phase 3:** `next-attach` + `url` + live-tweak + MCP Apps viewer/host + stage export.

**Top risks:**

1. **`vite-plugin-storybook-nextjs` coupling.** It lives in Storybook's monorepo and tracks Next internals. Mitigation: pin it, wrap behind `next-vite`, and keep the ability to fork. Its mocks are small.
2. **Meta-framework plugin filtering** is heuristic. Mitigation: a per-project override in `stage.json`, plus diagnostics that name the dropped plugins.
3. **Next lockfile format drift.** Mitigation: fall back to probing `package.json` `dev` port + HTTP check.
4. **Agents ignoring the skill.** Mitigation: layered instructions (§6.5) + result text that tells them the next step + S6 evals per agent.
5. **Edge missing** (EEA uninstall, non-Windows). Mitigation: Chrome detection → optional headless-shell download → degraded mode (in-app iframe diagnostics only).

---

## 11. Sources

**Tauri / wry / WebView2**
- Tauri config (`useHttpsScheme`, `additionalBrowserArgs`, CSP options): https://github.com/tauri-apps/tauri/blob/dev/crates/tauri-utils/src/config.rs
- Tauri CSP: https://v2.tauri.app/security/csp/ · isolation pattern: https://v2.tauri.app/concept/inter-process-communication/isolation/ · capabilities: https://v2.tauri.app/security/capabilities/
- Tauri IPC origin/local URL logic: https://github.com/tauri-apps/tauri/pull/9100 · https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/webview/mod.rs · iframe access note: https://github.com/tauri-apps/tauri/issues/8316
- Tauri Channels vs events: https://v2.tauri.app/develop/calling-frontend/ · https://github.com/tauri-apps/tauri/blob/dev/crates/tauri/src/ipc/channel.rs
- wry custom-protocol iframe support: https://github.com/tauri-apps/wry/pull/1491 · https://github.com/tauri-apps/wry/issues/1916 · IPC scripts in all frames: https://github.com/tauri-apps/wry/pull/1874 · https://github.com/tauri-apps/wry/blob/dev/src/webview2/mod.rs
- WebView2 `ICoreWebView2` (CapturePreview, CDP, filters): https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2 · Profile color scheme: https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2profile · Controller visibility: https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2controller · CDP for sessions: https://learn.microsoft.com/en-us/dotnet/api/microsoft.web.webview2.core.corewebview2.calldevtoolsprotocolmethodforsessionasync
- WebView2 hidden capture / offscreen: https://github.com/MicrosoftEdge/WebView2Feedback/issues/579 · https://github.com/MicrosoftEdge/WebView2Feedback/issues/547
- Playwright WebView2: https://playwright.dev/docs/webview2 · browser sizes: https://github.com/microsoft/playwright/blob/main/packages/playwright-core/browsers.json
- CDP Page/Emulation domains: https://github.com/ChromeDevTools/devtools-protocol/blob/master/pdl/domains/Page.pdl · https://github.com/ChromeDevTools/devtools-protocol/blob/master/pdl/domains/Emulation.pdl · Edge DevTools Protocol: https://learn.microsoft.com/microsoft-edge/devtools/protocol
- chromiumoxide: https://docs.rs/crate/chromiumoxide

**Web platform**
- HTML policy containers & sandboxing: https://html.spec.whatwg.org/multipage/browsers.html · iframe: https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe
- Iframe color-scheme propagation: https://developer.chrome.com/docs/modern-web-guidance/guides/visual-design/dark-mode · https://lists.w3.org/Archives/Public/public-css-archive/2026Apr/0698.html
- `*.localhost` loopback: https://groups.google.com/a/chromium.org/g/blink-dev/c/RC9dSw-O3fE
- Mixed content / potentially trustworthy: https://w3c.github.io/webappsec-mixed-content/ · https://w3c.github.io/webappsec-secure-contexts/#potentially-trustworthy-url

**Vite / React / Tailwind / Next / tooling**
- Vite 8: https://vite.dev/blog/announcing-vite8 · `resolve.tsconfigPaths`: https://vite.dev/config/shared-options#resolve-tsconfigpaths · JS API: https://vite.dev/guide/api-javascript · plugin API & virtual modules: https://vite.dev/guide/api-plugin · HMR API: https://vite.dev/guide/api-hmr · server options: https://raw.githubusercontent.com/vitejs/vite/main/docs/config/server-options.md · dep pre-bundling: https://vite.dev/guide/dep-pre-bundling · CSS preprocessors: https://vite.dev/config/shared-options#css-preprocessoroptions · CORS advisory: https://github.com/vitejs/vite/security/advisories/GHSA-vg6x-rcgg-rjx6 · tsconfig paths in CSS: https://github.com/vitejs/vite/pull/22775
- `@vitejs/plugin-react` changelog: https://github.com/vitejs/vite-plugin-react/blob/main/packages/plugin-react/CHANGELOG.md · `@vitejs/plugin-rsc`: https://github.com/vitejs/vite-plugin-react/tree/main/packages/plugin-rsc
- Storybook nextjs-vite: https://storybook.js.org/docs/get-started/frameworks/nextjs-vite · plugin source: https://github.com/storybookjs/storybook/blob/next/code/lib/vite-plugin-storybook-nextjs/src/index.ts · README: https://github.com/storybookjs/storybook/blob/next/code/lib/vite-plugin-storybook-nextjs/README.md · Storybook Vite builder: https://storybook.js.org/docs/builders/vite
- Ladle providers/config: https://ladle.dev/docs/providers · https://ladle.dev/docs/config · React Cosmos decorators/Vite/Next: https://reactcosmos.org/docs/fixtures/decorators · https://reactcosmos.org/docs/getting-started/vite · https://reactcosmos.org/docs/getting-started/next
- Tailwind source detection: https://tailwindcss.com/docs/detecting-classes-in-source-files · v4.1: https://tailwindcss.com/blog/tailwindcss-v4-1 · dark mode: https://tailwindcss.com/docs/dark-mode · Play CDN: https://tailwindcss.com/docs/installation/play-cdn · Vite plugin src: https://github.com/tailwindlabs/tailwindcss/blob/main/packages/@tailwindcss-vite/src/index.ts · PostCSS plugin src: https://github.com/tailwindlabs/tailwindcss/blob/main/packages/@tailwindcss-postcss/src/index.ts · Oxide scanner: https://github.com/tailwindlabs/tailwindcss/blob/main/crates/oxide/src/scanner/mod.rs
- Next 16: https://nextjs.org/blog/next-16 · lockfile test: https://github.com/vercel/next.js/blob/canary/test/development/lockfile/lockfile.test.ts · shared dev server PR: https://github.com/vercel/next.js/pull/100684 · `allowedDevOrigins`: https://nextjs.org/docs/app/api-reference/config/next-config-js/allowedDevOrigins · project structure: https://nextjs.org/docs/app/getting-started/project-structure · Turbopack: https://nextjs.org/docs/app/api-reference/turbopack · Sass: https://nextjs.org/docs/app/guides/sass · CVE-2026-27977: https://advisories.gitlab.com/npm/next/CVE-2026-27977/
- git `info/exclude`: https://git-scm.com/docs/gitignore
- TS dot-folder glob behavior: https://github.com/microsoft/TypeScript/blob/v5.9.3/src/compiler/utilities.ts#L9670
- React `_debugSource` removal: https://github.com/facebook/react/pull/28265 · DevTools source from stacks: https://github.com/facebook/react/pull/28351 · `captureOwnerStack`: https://react.dev/reference/react/captureOwnerStack · `createRoot` error options: https://react.dev/reference/react-dom/client/createRoot · `useSyncExternalStore`: https://react.dev/reference/react/useSyncExternalStore
- Click-to-source / agent feedback prior art: https://github.com/zh-lx/code-inspector · https://www.locatorjs.com/install/react · https://github.com/aidenybai/react-grab · https://github.com/aidenybai/bippy · Agentation: https://raw.githubusercontent.com/benjitaylor/agentation/main/README.md · Onlook: https://github.com/onlook-dev/onlook
- Oxc transformer: https://oxc.rs/docs/guide/usage/transformer · https://oxc.rs/docs/guide/usage/transformer/jsx · https://docs.rs/oxc_transformer · benchmarks: https://oxc.rs/blog/2024-09-29-transformer-alpha
- Sandpack bundler hosting (prior art for in-browser bundling): https://sandpack.codesandbox.io/docs/guides/hosting-the-bundler
- Streaming UI: Streamdown https://streamdown.ai/docs · https://vercel.com/changelog/introducing-streamdown · TanStack Virtual chat: https://tanstack.com/virtual/latest/docs/chat · virtua: https://raw.githubusercontent.com/inokawa/virtua/main/README.md · Zustand transient updates: https://deepwiki.com/pmndrs/zustand/7.4-transient-updates
- next-themes: https://github.com/shuding/next-themes

**MCP / agents**
- MCP versioning: https://modelcontextprotocol.io/specification/versioning · 2026-07-28 changelog: https://modelcontextprotocol.io/specification/2026-07-28/changelog · tools: https://modelcontextprotocol.io/specification/2026-07-28/server/tools · Streamable HTTP: https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http · 2025-11-25 transports: https://modelcontextprotocol.io/specification/2025-11-25/basic/transports · tasks: https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks
- MCP Apps: https://blog.modelcontextprotocol.io/posts/2026-01-26-mcp-apps/ · https://modelcontextprotocol.io/extensions/apps/overview · spec: https://raw.githubusercontent.com/modelcontextprotocol/ext-apps/main/specification/2026-01-26/apps.mdx · Skills extension: https://modelcontextprotocol.io/extensions/skills/overview
- Claude Code: CLI reference https://code.claude.com/docs/en/cli-reference · MCP https://code.claude.com/docs/en/mcp · skills https://code.claude.com/docs/en/skills · headless https://code.claude.com/docs/en/headless · memory https://code.claude.com/docs/en/memory
- Codex: MCP https://learn.chatgpt.com/docs/extend/mcp?surface=cli · config reference https://learn.chatgpt.com/docs/config-file/config-reference · app-server https://learn.chatgpt.com/docs/app-server · skills https://learn.chatgpt.com/docs/build-skills · app-server README https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md · MCP image handling https://github.com/openai/codex/blob/main/codex-rs/core/src/mcp_tool_call.rs · local schemas: `docs/research/schemas/codex/v2/{ThreadStartParams,SkillsExtraRootsSetParams}.json`
- Antigravity: MCP https://antigravity.google/docs/mcp · CLI reference https://antigravity.google/docs/cli/reference · ACP https://antigravity.google/docs/cli/acp.md · skills https://antigravity.google/docs/skills · rules https://antigravity.google/docs/rules
- ACP: session setup https://agentclientprotocol.com/protocol/session-setup · initialization https://agentclientprotocol.com/protocol/initialization
- Agent Skills: https://agentskills.io/specification · https://agentskills.io/client-implementation/adding-skills-support
- Claude vision sizing: https://platform.claude.com/docs/en/build-with-claude/vision
