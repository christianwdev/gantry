use std::env;
use std::path::PathBuf;

fn main() {
    // tauri-build only links its Windows resource (and with it the app manifest) into
    // `[[bin]]` targets. Test executables that link tauri then fail to load with
    // STATUS_ENTRYPOINT_NOT_FOUND because comctl32 v6 (`TaskDialogIndirect`) is not
    // activated. On MSVC we therefore skip tauri-build's manifest and embed the same one for
    // every target via the linker. Other targets keep tauri-build's default (the linker flags
    // below are MSVC-only), so they never end up with no manifest at all.
    let msvc = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let attributes = if msvc {
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest())
    } else {
        tauri_build::Attributes::new()
    };
    tauri_build::try_build(attributes).expect("tauri-build failed");
    if msvc {
        embed_windows_manifest();
    }
}

fn embed_windows_manifest() {
    let manifest_dir =
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let manifest = PathBuf::from(manifest_dir).join("windows-app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
}
