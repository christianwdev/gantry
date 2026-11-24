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
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RegGetValueW,
};

use crate::{Error, Result};

const EXECUTABLE_EXTS: [&str; 2] = ["exe", "com"];
const SHIM_EXTS: [&str; 4] = ["cmd", "bat", "ps1", "vbs"];

/// Returns the process `PATH` with any registry `PATH` entries it is missing appended
/// (machine entries first, then user entries — the order Windows itself composes them).
pub fn merged_path() -> OsString {
    let process = std::env::var_os("PATH").unwrap_or_default();
    let machine = registry_path(
        HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    );
    let user = registry_path(HKEY_CURRENT_USER, "Environment");
    merge_path_lists(&[
        process,
        machine.unwrap_or_default(),
        user.unwrap_or_default(),
    ])
}

/// Joins `;`-separated path lists, dropping empty and duplicate entries (case-insensitive,
/// ignoring trailing separators) while preserving first-seen order.
pub fn merge_path_lists(lists: &[OsString]) -> OsString {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<OsString> = Vec::new();
    for list in lists {
        for entry in split_path_list(list) {
            let key = entry
                .to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .to_lowercase();
            if !seen.contains(&key) {
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

fn registry_path(root: HKEY, subkey: &str) -> Option<OsString> {
    let subkey = wide_nul(OsStr::new(subkey));
    let value = wide_nul(OsStr::new("Path"));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut buf: Vec<u16> = vec![0; 2048];
    // Retry on ERROR_MORE_DATA: the value can grow between calls (an installer editing PATH).
    for _ in 0..8 {
        let mut size_bytes = (buf.len() * 2) as u32;
        // SAFETY: valid NUL-terminated strings; `buf` is writable for `size_bytes` bytes.
        let rc = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size_bytes,
            )
        };
        match rc {
            0 => {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                return Some(expand_env(&buf[..len]));
            }
            ERROR_MORE_DATA => buf = vec![0; (size_bytes as usize).div_ceil(2) + 64],
            _ => return None,
        }
    }
    None
}

fn expand_env(raw: &[u16]) -> OsString {
    let mut src = raw.to_vec();
    src.push(0);
    let mut cap = src.len() + 256;
    loop {
        let mut dst: Vec<u16> = vec![0; cap];
        // SAFETY: `src` is NUL-terminated; `dst` is writable for `cap` u16s.
        let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), dst.as_mut_ptr(), cap as u32) }
            as usize;
        if n == 0 {
            return OsString::from_wide(raw);
        }
        if n <= cap {
            return OsString::from_wide(&dst[..n.saturating_sub(1)]);
        }
        cap = n;
    }
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
///   extension tries `.exe` then `.com` in each directory. Script extensions (`.cmd`, `.bat`,
///   `.ps1`, `.vbs`) are always refused.
pub fn find_executable(
    program: &OsStr,
    search_path: &OsStr,
    cwd: Option<&Path>,
) -> Result<PathBuf> {
    let as_path = Path::new(program);
    let is_path = program.to_string_lossy().contains(['\\', '/', ':']);
    if is_path {
        let full = match cwd {
            Some(dir) if as_path.is_relative() => dir.join(as_path),
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

/// `symlink_metadata` rather than `metadata`: App Execution Aliases under `WindowsApps` are
/// zero-byte reparse points that `metadata`/`canonicalize` cannot follow, yet launch fine.
fn is_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| !m.is_dir())
        .unwrap_or(false)
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

    #[test]
    fn merges_and_dedupes_path_lists() {
        let merged = merge_path_lists(&[
            OsString::from(r"C:\A;c:\b\;;"),
            OsString::from(r#"C:\B;"C:\C";C:\a\"#),
        ]);
        assert_eq!(merged, OsString::from(r"C:\A;c:\b\;C:\C"));
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
