# Gantry

A lean, local-first Tauri v2 desktop cockpit for orchestrating **Claude Code**, **Codex**, and **Antigravity** agents — with a first-class **Design Stage** for rendering live HTML mockups and real React components from your active workspace.

## Status

Building, hard problems first (see [`docs/ROADMAP.md`](docs/ROADMAP.md)).

| Milestone                                                                                                | State                                                                                                                                                                                                                   |
| :------------------------------------------------------------------------------------------------------- | :---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Monorepo skeleton (Cargo + Bun workspaces, Tauri v2 shell, React 19 frontend, tauri-specta bindings, CI) | ✅                                                                                                                                                                                                                      |
| **X1: Windows process spine** (`crates/gantry-proc`)                                                     | ✅ `taskkill /F` of the host leaves 0 survivors across Node, headless Edge and Vite trees (21 processes), with 0 console windows and breakaway denied                                                                   |
| **X2: Claude Code worker** (`crates/gantry-providers`)                                                   | ✅ warm `stream-json` worker: 5+ turns, approvals (allow/deny/plan), AskUserQuestion, interrupt (running tool and pending approval), live model/mode switch, cross-cwd resume + fork; 18 replay tests run offline in CI |
| X3–X6, S1–S4 spikes                                                                                      | next                                                                                                                                                                                                                    |

## Docs

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — Unified architecture specification
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — Hard-stuff-first implementation roadmap
- [`docs/research/01-runtime-and-protocols.md`](docs/research/01-runtime-and-protocols.md) — Multi-provider runtime, wire protocols, process lifecycle, Windows systems architecture
- [`docs/research/02-design-canvas-and-frontend.md`](docs/research/02-design-canvas-and-frontend.md) — Live HTML & React component canvas, local MCP server, agent skill contract, frontend architecture

## Layout

```text
crates/gantry-proc/     Process spine: Job Objects, CREATE_NO_WINDOW, handle lists, .exe resolver, NDJSON framing
crates/gantry-*/        Placeholder crates for the subsystems in ARCHITECTURE.md §2
src-tauri/              Tauri v2 shell (single instance, IPC commands, binding export)
apps/desktop/           React 19 + Vite 8 frontend
packages/protocol/      Generated TypeScript bindings (do not edit by hand)
```

## Development

Prerequisites: Windows 11, Rust (pinned by `rust-toolchain.toml`), Bun 1.4+, Node 24+.

```sh
bun install
bun run dev                 # tauri dev: Vite on :1420 + the Gantry window
cargo test --workspace      # also regenerates packages/protocol/src/bindings.ts
cargo clippy --workspace --all-targets
bun run typecheck && bun run lint && bun run fmt:check

# X1 real-world containment check (launches node, headless Edge and Vite):
cargo test -p gantry-proc --test x1_containment -- --ignored --nocapture
```

Every child process must go through `gantry_proc::ManagedCommand`; `std::process::Command` and `tokio::process::Command` are banned by `clippy.toml`.
