use std::path::{Path, PathBuf};

/// Canonicalizes a path for use as a stable identity key (workspace roots, runner keys).
///
/// Uses `dunce::canonicalize`, which returns a plain `C:\...` path whenever that is safe and
/// only keeps the `\\?\` verbatim form when simplifying would change meaning (paths longer than
/// `MAX_PATH`, reserved names like `CON`, components with trailing dots/spaces). Callers that
/// hand paths to external tools should be aware such paths may break them. In both forms the
/// drive letter is lower-cased, so `C:\x` and `c:\x` (and `\\?\C:\x` / `\\?\c:\x`) produce the
/// same string.
///
/// Compare keys as strings (`as_os_str()`), not as `Path`s: `Path` equality already ignores
/// drive-letter case, but hashing/serializing the raw string does not.
pub fn normalize_path(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let canonical = dunce::canonicalize(path)?;
    Ok(lowercase_drive_letter(canonical))
}

/// The 4-byte verbatim prefix `\\?\`.
const VERBATIM_PREFIX: &[u8] = br"\\?\";

fn lowercase_drive_letter(path: PathBuf) -> PathBuf {
    let s = path.as_os_str().as_encoded_bytes();
    // `C:...` or verbatim `\\?\C:...`.
    let drive = if s.starts_with(VERBATIM_PREFIX) {
        VERBATIM_PREFIX.len()
    } else {
        0
    };
    if s.len() >= drive + 2 && s[drive + 1] == b':' && s[drive].is_ascii_uppercase() {
        let mut bytes = s.to_vec();
        bytes[drive] = bytes[drive].to_ascii_lowercase();
        // SAFETY: we only replaced one ASCII byte with another ASCII byte, which preserves the
        // validity of the platform's OsStr encoding (WTF-8 on Windows).
        let os = unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(bytes) };
        return PathBuf::from(os);
    }
    path
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn verbatim_prefix_is_four_bytes() {
        // Escaped (non-raw) literal as an independent spelling of the same 4 bytes.
        assert_eq!(VERBATIM_PREFIX, b"\\\\?\\");
        assert_eq!(VERBATIM_PREFIX.len(), 4);
    }

    #[test]
    fn lowercases_drive_and_avoids_verbatim() {
        let p = normalize_path(std::env::temp_dir()).unwrap();
        let s = p.to_string_lossy();
        assert!(!s.starts_with(r"\\?\"), "{s}");
        assert!(s.as_bytes()[0].is_ascii_lowercase(), "{s}");
        assert_eq!(s.as_bytes()[1], b':');
    }

    /// Compares raw strings: `PathBuf` equality would pass regardless, because std's prefix
    /// parsing upper-cases the drive letter of `Disk`/`VerbatimDisk` prefixes.
    #[test]
    fn lowercases_drive_inside_verbatim_prefix() {
        let out = lowercase_drive_letter(PathBuf::from(r"\\?\C:\very\long\CON"));
        assert_eq!(out.as_os_str(), OsStr::new(r"\\?\c:\very\long\CON"));
        let out = lowercase_drive_letter(PathBuf::from(r"\\?\UNC\server\share"));
        assert_eq!(out.as_os_str(), OsStr::new(r"\\?\UNC\server\share"));
        let out = lowercase_drive_letter(PathBuf::from(r"D:\x"));
        assert_eq!(out.as_os_str(), OsStr::new(r"d:\x"));
    }
}
