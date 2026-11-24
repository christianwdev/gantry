use std::env;
use std::path::PathBuf;

fn main() {
    // tauri-build only links its Windows resource (and with it the app manifest) into
    // `[[bin]]` targets. Test executables that link tauri then fail to load with
    // STATUS_ENTRYPOINT_NOT_FOUND because comctl32 v6 (`TaskDialogIndirect`) is not
    // activated. So skip tauri-build's manifest and embed the same one for every target.
    let windows = tauri_build::WindowsAttributes::new_without_app_manifest();
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("tauri-build failed");
    embed_windows_manifest();
}

fn embed_windows_manifest() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let manifest_dir =
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let manifest = PathBuf::from(manifest_dir).join("windows-app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
}
