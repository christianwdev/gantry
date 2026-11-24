use std::path::{Path, PathBuf};

/// Canonicalizes a path for use as a stable identity key (workspace roots, runner keys).
///
/// Uses `dunce::canonicalize` so the result is never a `\\?\` verbatim path (which breaks many
/// tools and dev servers), then lower-cases the drive letter so `C:\x` and `c:\x` compare equal.
pub fn normalize_path(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let canonical = dunce::canonicalize(path)?;
    Ok(lowercase_drive_letter(canonical))
}

fn lowercase_drive_letter(path: PathBuf) -> PathBuf {
    let s = path.as_os_str().as_encoded_bytes();
    if s.len() >= 2 && s[1] == b':' && s[0].is_ascii_uppercase() {
        let mut bytes = s.to_vec();
        bytes[0] = bytes[0].to_ascii_lowercase();
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
}
