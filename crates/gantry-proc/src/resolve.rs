//! Real-executable resolution and registry `PATH` merging.
//!
//! Gantry is often launched from the Start menu or a long-lived Explorer session whose inherited
//! `PATH` predates a CLI install (e.g. the user just installed `agy`). We therefore merge the live
//! registry `PATH` (machine + user) behind the process `PATH` before resolving programs and pass
//! the merged value to children.
//!
//! Only `.exe` and `.com` files are ever returned. A program that only exists as a `.cmd`/`.bat`
//! /`.ps1` shim yields [`Error::ShimOnly`]: launching through `cmd.exe` would reintroduce argument
//! injection (BatBadBut) and TOML/JSON quote mangling.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, REG_VALUE_TYPE, RRF_NOEXPAND,
    RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
};

use crate::cmdline::env_key_cmp;
use crate::{Error, Result};

/// Search order within one directory, matching the default `PATHEXT` (`.COM;.EXE;...`).
const EXECUTABLE_EXTS: [&str; 2] = ["com", "exe"];
const SHIM_EXTS: [&str; 4] = ["cmd", "bat", "ps1", "vbs"];

/// Returns the process `PATH` with any registry `PATH` entries it is missing appended
/// (machine entries first, then user entries — the order Windows itself composes them).
pub fn merged_path() -> OsString {
    let process = std::env::var_os("PATH").unwrap_or_default();
    let machine = registry_path(HKEY_LOCAL_MACHINE, MACHINE_ENV, None);
    let user = registry_path(HKEY_CURRENT_USER, USER_ENV, machine.as_deref());
    merge_path_lists(&[
        process,
        machine.unwrap_or_default(),
        user.unwrap_or_default(),
    ])
}

/// Joins `;`-separated path lists, dropping empty and duplicate entries (case-insensitive,
/// ignoring trailing separators) while preserving first-seen order.
pub fn merge_path_lists(lists: &[OsString]) -> OsString {
    // Keys are the raw UTF-16 without trailing separators, compared with the OS's ordinal
    // case-insensitive rules (no lossy conversion, so distinct directories never collide).
    let mut seen: Vec<OsString> = Vec::new();
    let mut out: Vec<OsString> = Vec::new();
    for list in lists {
        for entry in split_path_list(list) {
            let mut wide: Vec<u16> = entry.encode_wide().collect();
            // Strip trailing separators, but never the root separator of `X:\`: `C:` (the
            // drive's current directory) and `C:\` (its root) are different directories.
            let is_drive_root = |w: &[u16]| w.len() == 3 && w[1] == u16::from(b':');
            while wide.len() > 1
                && matches!(wide.last(), Some(&0x5C | &0x2F))
                && !is_drive_root(&wide)
            {
                wide.pop();
            }
            let key = OsString::from_wide(&wide);
            if !seen.iter().any(|k| env_key_cmp(k, &key).is_eq()) {
                seen.push(key);
                out.push(entry);
            }
        }
    }
    let mut joined = OsString::new();
    for (i, entry) in out.iter().enumerate() {
        if i > 0 {
            joined.push(";");
        }
        // An entry that itself contains ';' must be quoted to survive re-splitting.
        if entry.encode_wide().any(|c| c == SEMI) {
            joined.push("\"");
            joined.push(entry);
            joined.push("\"");
        } else {
            joined.push(entry);
        }
    }
    joined
}

const SEMI: u16 = b';' as u16;
const DQUOTE: u16 = b'"' as u16;

/// Splits a `;`-separated path list the way cmd.exe does: a `;` inside double quotes is part of
/// the entry, and the quotes themselves are removed. Whitespace is trimmed. Works on raw UTF-16
/// so entries containing unpaired surrogates survive intact.
fn split_path_list(list: &OsStr) -> impl Iterator<Item = OsString> + '_ {
    let mut entries: Vec<Vec<u16>> = vec![Vec::new()];
    let mut quoted = false;
    for c in list.encode_wide() {
        match c {
            DQUOTE => quoted = !quoted,
            SEMI if !quoted => entries.push(Vec::new()),
            _ => entries.last_mut().expect("never empty").push(c),
        }
    }
    let is_space = |c: &u16| matches!(*c, 0x20 | 0x09 | 0x0A | 0x0D);
    entries
        .into_iter()
        .map(move |part| {
            let start = part.iter().position(|c| !is_space(c)).unwrap_or(part.len());
            let end = part
                .iter()
                .rposition(|c| !is_space(c))
                .map_or(start, |i| i + 1);
            OsString::from_wide(&part[start..end])
        })
        .filter(|s| !s.is_empty())
}

const MACHINE_ENV: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
const USER_ENV: &str = "Environment";

/// A string read from the registry or environment, and whether it may contain `%VAR%`
/// references to expand (only `REG_EXPAND_SZ` does; `REG_SZ` and already-expanded process
/// environment values are literal, as Windows treats them).
type VarValue = (Vec<u16>, bool);

/// Reads a `REG_SZ`/`REG_EXPAND_SZ` value *unexpanded* (no trailing NUL).
fn registry_value(root: HKEY, subkey: &str, name: &OsStr) -> Option<VarValue> {
    let subkey = wide_nul(OsStr::new(subkey));
    let value = wide_nul(name);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut buf: Vec<u16> = vec![0; 2048];
    // Retry on ERROR_MORE_DATA: the value can grow between calls (an installer editing PATH).
    for _ in 0..8 {
        let mut size_bytes = (buf.len() * 2) as u32;
        let mut kind: REG_VALUE_TYPE = 0;
        // SAFETY: valid NUL-terminated strings; `buf` is writable for `size_bytes` bytes.
        let rc = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                value.as_ptr(),
                flags,
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut size_bytes,
            )
        };
        match rc {
            0 => {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                buf.truncate(len);
                return Some((buf, kind == REG_EXPAND_SZ));
            }
            ERROR_MORE_DATA => buf = vec![0; (size_bytes as usize).div_ceil(2) + 64],
            _ => return None,
        }
    }
    None
}

/// Reads a registry `PATH` value and expands `%VARS%` against the **live** registry
/// environment (user, then machine) before Gantry's own environment.
///
/// `ExpandEnvironmentStringsW` would use Gantry's process environment, which is exactly what
/// is stale after an install: an installer that adds `NVM_SYMLINK` / `PNPM_HOME` and appends
/// `%NVM_SYMLINK%` to PATH would otherwise leave a literal, unusable entry.
///
/// Scoping follows Windows: the machine `Path` is expanded with system variables, the user
/// `Path` with user variables first, then system ones. Gantry's own environment is the final
/// fallback, needed for built-in dynamic variables that are not in either registry key
/// (`SystemRoot`, `USERPROFILE`, `APPDATA`, ...). Approximation: for the machine `Path` that
/// fallback can also supply a user-only variable Windows would leave unexpanded — harmless,
/// since such an entry would otherwise be a literal `%VAR%` that is skipped anyway.
///
/// Only `REG_EXPAND_SZ` values are expanded (recursively); `REG_SZ` values and process
/// environment values are used literally, as Windows does.
///
/// `%PATH%` inside a `Path` value refers to the *previous* PATH, not to the value itself (some
/// installers write a user Path of `%PATH%;C:\tools`): it resolves to `prior` (the expanded
/// machine Path when expanding the user Path), and stays literal when there is none.
fn registry_path(root: HKEY, subkey: &str, prior: Option<&OsStr>) -> Option<OsString> {
    let (raw, expandable) = registry_value(root, subkey, OsStr::new("Path"))?;
    if !expandable {
        return Some(OsString::from_wide(&raw));
    }
    let lookup = env_lookup(root == HKEY_CURRENT_USER, prior);
    Some(OsString::from_wide(&expand_vars(&raw, &lookup, 4)))
}

/// The variable lookup used when expanding a registry `Path` (see [`registry_path`]).
fn env_lookup(is_user: bool, prior: Option<&OsStr>) -> impl Fn(&OsStr) -> Option<VarValue> + '_ {
    move |name: &OsStr| {
        if name.eq_ignore_ascii_case("PATH") {
            return prior.map(|p| (p.encode_wide().collect(), false));
        }
        is_user
            .then(|| registry_value(HKEY_CURRENT_USER, USER_ENV, name))
            .flatten()
            .or_else(|| registry_value(HKEY_LOCAL_MACHINE, MACHINE_ENV, name))
            .or_else(|| std::env::var_os(name).map(|v| (v.encode_wide().collect(), false)))
    }
}

/// Expands `%NAME%` references using `lookup`, recursing up to `depth` levels into values that
/// are themselves expandable (`REG_EXPAND_SZ`); literal values are inserted as-is. Unknown names
/// stay literal (`%NAME%`) and scanning resumes at their closing `%`, as
/// `ExpandEnvironmentStringsW` does.
fn expand_vars(raw: &[u16], lookup: &dyn Fn(&OsStr) -> Option<VarValue>, depth: u8) -> Vec<u16> {
    const PCT: u16 = b'%' as u16;
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != PCT {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        let Some(len) = raw[i + 1..].iter().position(|&c| c == PCT) else {
            out.extend_from_slice(&raw[i..]);
            break;
        };
        let name = &raw[i + 1..i + 1 + len];
        let value = if name.is_empty() {
            None
        } else {
            lookup(&OsString::from_wide(name))
        };
        match value {
            Some((v, true)) if depth > 0 => out.extend(expand_vars(&v, lookup, depth - 1)),
            Some((v, _)) => out.extend(v),
            None => {
                // Keep `%NAME` and let the closing `%` start the next candidate.
                out.extend_from_slice(&raw[i..i + 1 + len]);
                i += 1 + len;
                continue;
            }
        }
        i += len + 2;
    }
    out
}

pub(crate) fn wide_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Resolves `program` to a real `.exe`/`.com`.
///
/// * Paths (containing a separator or drive) are resolved against `cwd` and must point at an
///   executable; an extension-less path tries `.exe`.
/// * Bare names are searched only in the absolute directories of `search_path` (never the
///   current directory, and relative PATH entries are skipped). A bare name without an
///   extension tries `.com` then `.exe` in each directory (default `PATHEXT` order). Script
///   extensions (`.cmd`, `.bat`, `.ps1`, `.vbs`) are always refused.
pub fn find_executable(
    program: &OsStr,
    search_path: &OsStr,
    cwd: Option<&Path>,
) -> Result<PathBuf> {
    let as_path = Path::new(program);
    let is_path = program.to_string_lossy().contains(['\\', '/', ':']);
    if is_path {
        // Drive-relative (`C:tool.exe`: a drive prefix but no root) would resolve against
        // Gantry's own per-drive cwd, and `Path::join` with it discards `cwd` entirely. Refuse.
        let has_prefix = matches!(as_path.components().next(), Some(Component::Prefix(_)));
        if has_prefix && !as_path.has_root() {
            return Err(Error::DriveRelativePath {
                path: as_path.to_path_buf(),
            });
        }
        let full = match cwd {
            // A relative cwd gives no fixed base either (it is itself relative to Gantry's cwd).
            Some(dir) if as_path.is_relative() && dir.is_absolute() => dir.join(as_path),
            Some(_) if as_path.is_relative() => {
                return Err(Error::RelativeWithoutCwd {
                    path: as_path.to_path_buf(),
                });
            }
            // Without a cwd a relative program path would resolve against Gantry's own
            // (arbitrary) working directory — the same planted-exe risk as relative PATH
            // entries. Require an explicit base.
            None if as_path.is_relative() => {
                return Err(Error::RelativeWithoutCwd {
                    path: as_path.to_path_buf(),
                });
            }
            _ => as_path.to_path_buf(),
        };
        return check_explicit(full);
    }

    let ext = extension_lower(as_path);
    if ext.as_deref().is_some_and(|e| SHIM_EXTS.contains(&e)) {
        // Refused regardless of PATH or existence: Gantry never launches scripts.
        return Err(Error::UnsupportedExecutable {
            path: as_path.to_path_buf(),
        });
    }
    let mut shim: Option<PathBuf> = None;
    for dir in split_path_list(search_path) {
        let dir = PathBuf::from(dir);
        // Relative entries (".", "bin") would resolve against Gantry's own cwd, which is
        // neither intended nor safe (a planted git.exe); Windows' loader has the same flaw.
        if !dir.is_absolute() {
            continue;
        }
        match ext.as_deref() {
            Some(e) if EXECUTABLE_EXTS.contains(&e) => {
                let candidate = dir.join(as_path);
                if is_file(&candidate) {
                    return Ok(dunce::simplified(&candidate).to_path_buf());
                }
            }
            _ => {
                for e in EXECUTABLE_EXTS {
                    let candidate = with_extra_ext(&dir.join(as_path), e);
                    if is_file(&candidate) {
                        return Ok(dunce::simplified(&candidate).to_path_buf());
                    }
                }
                if shim.is_none() {
                    shim = SHIM_EXTS
                        .iter()
                        .map(|e| with_extra_ext(&dir.join(as_path), e))
                        .find(|c| is_file(c));
                }
            }
        }
    }
    match shim {
        Some(shim) => Err(Error::ShimOnly {
            program: program.to_owned(),
            shim,
        }),
        None => Err(Error::NotFound {
            program: program.to_owned(),
        }),
    }
}

fn check_explicit(path: PathBuf) -> Result<PathBuf> {
    match extension_lower(&path).as_deref() {
        Some(e) if EXECUTABLE_EXTS.contains(&e) => {
            if is_file(&path) {
                Ok(dunce::simplified(&path).to_path_buf())
            } else {
                Err(Error::NotFound {
                    program: path.into_os_string(),
                })
            }
        }
        Some(e) if SHIM_EXTS.contains(&e) => Err(Error::UnsupportedExecutable { path }),
        _ => {
            let with_exe = with_extra_ext(&path, "exe");
            if is_file(&with_exe) {
                Ok(dunce::simplified(&with_exe).to_path_buf())
            } else if is_file(&path) {
                Err(Error::UnsupportedExecutable { path })
            } else {
                Err(Error::NotFound {
                    program: path.into_os_string(),
                })
            }
        }
    }
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// Appends `.ext` without replacing an existing dotted suffix (`node18` → `node18.exe`,
/// `foo.bar` → `foo.bar.exe`).
fn with_extra_ext(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Whether `path` is something `CreateProcessW` can launch as a file.
///
/// * Real symlinks / junctions (`is_symlink`) must resolve to a file: a dangling link or a
///   junction to a directory is skipped so the search continues to the next PATH entry.
/// * Other reparse points are accepted via `symlink_metadata`: App Execution Aliases under
///   `WindowsApps` are zero-byte `IO_REPARSE_TAG_APPEXECLINK` files that `metadata` cannot
///   follow, yet launch fine.
fn is_file(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::metadata(path).is_ok_and(|target| target.is_file())
        }
        Ok(m) => !m.is_dir(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system32() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32")
    }

    #[test]
    fn finds_system_exe_by_bare_name() {
        let found = find_executable(OsStr::new("cmd"), system32().as_os_str(), None).unwrap();
        assert!(
            found
                .to_string_lossy()
                .to_lowercase()
                .ends_with(r"system32\cmd.exe"),
            "{found:?}"
        );
        let found = find_executable(OsStr::new("CMD.EXE"), system32().as_os_str(), None).unwrap();
        assert!(found.is_file());
    }

    #[test]
    fn reports_shim_only_and_not_found() {
        let dir = std::env::temp_dir().join(format!("gantry-proc-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("onlyshim.cmd"), "@echo off\r\n").unwrap();
        let err = find_executable(OsStr::new("onlyshim"), dir.as_os_str(), None).unwrap_err();
        assert!(matches!(err, Error::ShimOnly { .. }), "{err:?}");
        let err = find_executable(OsStr::new("definitely-missing-xyz"), dir.as_os_str(), None)
            .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
        let err = find_executable(dir.join("onlyshim.cmd").as_os_str(), OsStr::new(""), None)
            .unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedExecutable { .. }),
            "{err:?}"
        );
        let err = find_executable(OsStr::new("onlyshim.cmd"), dir.as_os_str(), None).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedExecutable { .. }),
            "{err:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_program_paths_need_a_cwd() {
        let err = find_executable(OsStr::new(r"bin\tool.exe"), OsStr::new(""), None).unwrap_err();
        assert!(matches!(err, Error::RelativeWithoutCwd { .. }), "{err:?}");
        // A relative cwd is no fixed base either.
        let err = find_executable(
            OsStr::new(r"bin\tool.exe"),
            OsStr::new(""),
            Some(Path::new("proj")),
        )
        .unwrap_err();
        assert!(matches!(err, Error::RelativeWithoutCwd { .. }), "{err:?}");
        // With an absolute cwd it is resolved against it (and here simply not found).
        let err = find_executable(
            OsStr::new(r"bin\tool.exe"),
            OsStr::new(""),
            Some(&std::env::temp_dir()),
        )
        .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn drive_relative_program_paths_are_refused() {
        for p in [r"C:tool.exe", r"C:bin\tool.exe"] {
            let err = find_executable(OsStr::new(p), OsStr::new(""), Some(Path::new(r"C:\work")))
                .unwrap_err();
            assert!(
                matches!(err, Error::DriveRelativePath { .. }),
                "{p}: {err:?}"
            );
        }
    }

    #[test]
    fn script_extensions_are_refused_even_with_empty_path() {
        for path in [OsStr::new(""), OsStr::new(r"C:\Windows\System32")] {
            let err = find_executable(OsStr::new("typo.cmd"), path, None).unwrap_err();
            assert!(
                matches!(err, Error::UnsupportedExecutable { .. }),
                "{err:?}"
            );
        }
    }

    #[test]
    fn relative_path_entries_are_skipped() {
        // Plant an exe in a directory reachable via a *relative* PATH entry from the test's
        // cwd (no set_current_dir: tests run in parallel threads of one process).
        let rel = format!("target-rel-path-test-{}", std::process::id());
        let dir = std::env::current_dir().unwrap().join(&rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(system32().join("cmd.exe"), dir.join("planted.exe")).unwrap();
        let res = find_executable(OsStr::new("planted"), OsStr::new(&format!(".;{rel}")), None);
        // Control: the same directory as an absolute entry is found.
        let abs = find_executable(OsStr::new("planted"), dir.as_os_str(), None);
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(res, Err(Error::NotFound { .. })), "{res:?}");
        assert!(abs.is_ok(), "{abs:?}");
    }

    #[test]
    fn split_trims_on_utf16_and_keeps_lone_surrogates() {
        let mut raw: Vec<u16> = r#" "C:\x"#.encode_utf16().collect();
        raw.push(0xD800); // unpaired surrogate
        raw.extend(r#"" ;C:\y"#.encode_utf16());
        let parts: Vec<OsString> = split_path_list(&OsString::from_wide(&raw)).collect();
        let mut expected: Vec<u16> = r"C:\x".encode_utf16().collect();
        expected.push(0xD800);
        assert_eq!(
            parts,
            vec![OsString::from_wide(&expected), OsString::from(r"C:\y")]
        );
    }

    #[test]
    fn quoted_semicolons_stay_in_one_entry_and_survive_rejoining() {
        let list = OsString::from(r#"C:\a;"C:\Tools;v2\bin";C:\b"#);
        let parts: Vec<OsString> = split_path_list(&list).collect();
        assert_eq!(
            parts,
            vec![
                OsString::from(r"C:\a"),
                OsString::from(r"C:\Tools;v2\bin"),
                OsString::from(r"C:\b"),
            ]
        );
        let merged = merge_path_lists(&[list]);
        assert_eq!(merged, OsString::from(r#"C:\a;"C:\Tools;v2\bin";C:\b"#));
        assert_eq!(split_path_list(&merged).count(), 3);
    }

    #[tokio::test]
    async fn junctions_to_directories_are_skipped_but_exec_aliases_resolve() {
        let root =
            std::env::temp_dir().join(format!("gantry-proc-junction-{}", std::process::id()));
        let (early, late, target) = (root.join("early"), root.join("late"), root.join("target"));
        for d in [&early, &late, &target] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(system32().join("cmd.exe"), late.join("tool.exe")).unwrap();
        // A junction (no privilege needed) named like the program, pointing at a directory.
        // Keep the child until it exits: dropping it would kill mklink via its job before the
        // junction exists (which silently made an earlier version of this test vacuous).
        let mut mklink = crate::ManagedCommand::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(early.join("tool.exe"))
            .arg(&target)
            .spawn()
            .unwrap();
        assert!(mklink.wait().await.unwrap().success(), "mklink /J failed");
        let junction = std::fs::symlink_metadata(early.join("tool.exe")).unwrap();
        assert!(
            junction.file_type().is_symlink(),
            "precondition: a junction named tool.exe must exist"
        );
        let mut search = early.clone().into_os_string();
        search.push(";");
        search.push(late.as_os_str());
        let found = find_executable(OsStr::new("tool"), &search, None);
        let _ = std::fs::remove_dir(early.join("tool.exe"));
        std::fs::remove_dir_all(&root).ok();
        assert!(
            found.as_ref().is_ok_and(|p| p.starts_with(&late)),
            "junction should be skipped: {found:?}"
        );

        // App Execution Aliases (zero-byte reparse points) must still resolve, if present.
        let apps = std::env::var_os("LOCALAPPDATA")
            .map(|l| PathBuf::from(l).join(r"Microsoft\WindowsApps"))
            .filter(|d| d.is_dir());
        let alias = apps.as_ref().and_then(|d| {
            std::fs::read_dir(d)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
                        && std::fs::symlink_metadata(p).is_ok_and(|m| m.len() == 0)
                })
        });
        if let (Some(dir), Some(alias)) = (apps, alias) {
            let name = alias.file_stem().unwrap().to_owned();
            assert!(
                find_executable(&name, dir.as_os_str(), None).is_ok(),
                "exec alias {alias:?} should resolve"
            );
        }
    }

    #[test]
    fn com_wins_over_exe_like_pathext() {
        let dir = std::env::temp_dir().join(format!("gantry-proc-com-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(system32().join("cmd.exe"), dir.join("tool.exe")).unwrap();
        std::fs::copy(system32().join("cmd.exe"), dir.join("tool.com")).unwrap();
        let found = find_executable(OsStr::new("tool"), dir.as_os_str(), None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found.unwrap().extension().unwrap(), "com");
    }

    #[test]
    fn drive_root_and_drive_relative_are_distinct() {
        let merged = merge_path_lists(&[OsString::from(r"C:;C:\;C:\\;D:\x\")]);
        let parts: Vec<OsString> = split_path_list(&merged).collect();
        assert_eq!(
            parts,
            vec![
                OsString::from("C:"),
                OsString::from(r"C:\"),
                OsString::from(r"D:\x\")
            ]
        );
    }

    #[test]
    fn dedupe_is_lossless_and_unicode_case_insensitive() {
        let mut a: Vec<u16> = r"C:\x\".encode_utf16().collect();
        a.push(0xD800);
        let mut b: Vec<u16> = r"C:\x\".encode_utf16().collect();
        b.push(0xDC00);
        let merged = merge_path_lists(&[
            OsString::from_wide(&a),
            OsString::from_wide(&b),
            OsString::from(r"C:\Äpfel"),
            OsString::from(r"c:\äpfel\"),
        ]);
        let parts: Vec<OsString> = split_path_list(&merged).collect();
        assert_eq!(
            parts,
            vec![
                OsString::from_wide(&a),
                OsString::from_wide(&b),
                OsString::from(r"C:\Äpfel"),
            ]
        );
    }

    #[test]
    fn merges_and_dedupes_path_lists() {
        let merged = merge_path_lists(&[
            OsString::from(r"C:\A;c:\b\;;"),
            OsString::from(r#"C:\B;"C:\C";C:\a\"#),
        ]);
        assert_eq!(merged, OsString::from(r"C:\A;c:\b\;C:\C"));
    }

    /// `(name, value, is REG_EXPAND_SZ)`.
    fn expand(raw: &str, vars: &[(&str, &str, bool)]) -> String {
        let lookup = |name: &OsStr| {
            vars.iter()
                .find(|(k, _, _)| OsStr::new(k).eq_ignore_ascii_case(name))
                .map(|(_, v, expandable)| (v.encode_utf16().collect(), *expandable))
        };
        let wide: Vec<u16> = raw.encode_utf16().collect();
        String::from_utf16(&expand_vars(&wide, &lookup, 4)).unwrap()
    }

    #[test]
    fn expands_vars_from_injected_environment() {
        let vars = [
            ("NVM_HOME", r"C:\nvm", false),
            ("NVM_SYMLINK", r"%NVM_HOME%\nodejs", true), // REG_EXPAND_SZ referencing a var
            ("LITERAL", r"C:\a%NVM_HOME%b", false),      // REG_SZ: '%' is literal
            ("LOOP", "%LOOP%", true),
        ];
        assert_eq!(expand("%LITERAL%", &vars), r"C:\a%NVM_HOME%b");
        assert_eq!(
            expand(r"%NVM_HOME%;%nvm_symlink%;C:\x", &vars),
            r"C:\nvm;C:\nvm\nodejs;C:\x"
        );
        // Unknown names stay literal; the closing % may start the next reference.
        assert_eq!(expand("%NOPE%NVM_HOME%", &vars), r"%NOPEC:\nvm");
        assert_eq!(
            expand("100%; %%; trailing %", &vars),
            "100%; %%; trailing %"
        );
        // Self-reference terminates (depth-limited) instead of recursing forever.
        assert_eq!(expand("%LOOP%", &vars), "%LOOP%");
    }

    #[test]
    fn user_path_self_reference_means_the_machine_path() {
        let machine = OsStr::new(r"C:\Windows\System32;C:\m");
        let user_raw: Vec<u16> = r"%PATH%;C:\tools".encode_utf16().collect();
        let expanded = expand_vars(&user_raw, &env_lookup(true, Some(machine)), 4);
        assert_eq!(
            String::from_utf16(&expanded).unwrap(),
            r"C:\Windows\System32;C:\m;C:\tools"
        );
        // Without a prior PATH (machine Path itself), %PATH% stays literal (then skipped).
        let expanded = expand_vars(&user_raw, &env_lookup(false, None), 4);
        assert_eq!(String::from_utf16(&expanded).unwrap(), r"%PATH%;C:\tools");
    }

    #[test]
    fn registry_path_is_merged() {
        let merged = merged_path();
        let lower = merged.to_string_lossy().to_lowercase();
        assert!(lower.contains("system32"), "{lower}");
        assert!(
            !lower.contains('%'),
            "registry PATH should be expanded: {lower}"
        );
    }
}
