//! Gantry desktop shell: Tauri v2 app setup, IPC commands and binding export.

mod health;

use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Manager};
use tauri_specta::{Builder, collect_commands};

/// Label of the primary window declared in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";

/// Where the generated TypeScript bindings are written (debug builds only).
#[cfg(debug_assertions)]
const BINDINGS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../packages/protocol/src/bindings.ts"
);

/// Basic facts about the running app and its host environment.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// Gantry's own version (from the Cargo package).
    pub version: String,
    /// Host operating system (`std::env::consts::OS`).
    pub os: String,
    /// Host CPU architecture (`std::env::consts::ARCH`).
    pub arch: String,
    /// Installed WebView runtime version, if it could be determined.
    pub webview_version: Option<String>,
}

/// Returns version and platform information for the "Environment" panel.
#[tauri::command]
#[specta::specta]
fn app_info(app: AppHandle) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        webview_version: tauri::webview_version().ok(),
    }
}

/// Probes the external CLIs Gantry drives (claude, codex, agy, git, node, bun).
#[tauri::command]
#[specta::specta]
async fn tool_health() -> Vec<health::ToolStatus> {
    health::probe_all().await
}

/// Builds the tauri-specta registry of every IPC command exposed to the frontend.
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![app_info, tool_health])
        // Equivalent of `BigIntExportBehavior::Number` (ARCHITECTURE.md §2).
        .dangerously_cast_bigints_to_number()
}

/// Brings the existing main window to the front (used when a second instance is launched).
fn focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        // Best effort: a failure here only means the window stays where it was.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Entry point shared by the `gantry` binary.
///
/// # Panics
///
/// Panics if the Tauri runtime fails to start, or (debug builds only) if the
/// TypeScript bindings cannot be exported.
pub fn run() {
    let builder = specta_builder();

    #[cfg(debug_assertions)]
    builder
        .export(specta_typescript::Typescript::default(), BINDINGS_PATH)
        .expect("failed to export TypeScript bindings");

    tauri::Builder::default()
        // Must be registered first so a second launch exits before doing any work.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            focus_main_window(app);
        }))
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Gantry");
}

#[cfg(test)]
mod tests {
    use super::{BINDINGS_PATH, specta_builder};

    /// Regenerates `packages/protocol/src/bindings.ts`. Exporting must succeed (catches
    /// unsupported types, e.g. bigints, before runtime), and CI runs `git diff --exit-code`
    /// afterwards so committed bindings can never drift from the Rust command surface.
    #[test]
    fn bindings_export() {
        specta_builder()
            .export(specta_typescript::Typescript::default(), BINDINGS_PATH)
            .expect("bindings export");
    }
}
