use std::path::{Path, PathBuf};

/// Canonicalizes a path for use as a stable identity key (workspace roots, runner keys).
///
/// Uses `dunce::canonicalize`, which returns a plain `C:\...` path whenever that is safe and
/// only keeps the `\?\` verbatim form when simplifying would change meaning (paths longer than
/// `MAX_PATH`, reserved names like `CON`, components with trailing dots/spaces). Callers that
/// hand paths to external tools should be aware such paths may break them. In both forms the
/// drive letter is lower-cased, so `C:\x` and `c:\x` (and `\?\C:\x` / `\?\c:\x`) compare equal.
pub fn normalize_path(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let canonical = dunce::canonicalize(path)?;
    Ok(lowercase_drive_letter(canonical))
}

fn lowercase_drive_letter(path: PathBuf) -> PathBuf {
    let s = path.as_os_str().as_encoded_bytes();
    // `C:...` or verbatim `\?\C:...`.
    let drive = if s.starts_with(br"\?\") { 4 } else { 0 };
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
    use super::*;

    #[test]
    fn lowercases_drive_and_avoids_verbatim() {
        let p = normalize_path(std::env::temp_dir()).unwrap();
        let s = p.to_string_lossy();
        assert!(!s.starts_with(r"\\?\"), "{s}");
        assert!(s.as_bytes()[0].is_ascii_lowercase(), "{s}");
        assert_eq!(s.as_bytes()[1], b':');
    }

    #[test]
    fn lowercases_drive_inside_verbatim_prefix() {
        assert_eq!(
            lowercase_drive_letter(PathBuf::from(r"\\?\C:\very\long\CON")),
            PathBuf::from(r"\\?\c:\very\long\CON")
        );
        assert_eq!(
            lowercase_drive_letter(PathBuf::from(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\?\UNC\server\share")
        );
    }
}
