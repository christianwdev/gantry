//! Pure helpers for building `CreateProcessW` inputs: the command line string (MSVCRT /
//! `CommandLineToArgvW` quoting rules) and the Unicode environment block.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use crate::{Error, Result};

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;

/// Builds a NUL-terminated command line: the quoted program path followed by each argument,
/// quoted so that the MSVCRT / `CommandLineToArgvW` parser reproduces it exactly.
pub(crate) fn command_line(program: &Path, args: &[OsString]) -> Result<Vec<u16>> {
    let mut out = Vec::with_capacity(256);
    let prog: Vec<u16> = program.as_os_str().encode_wide().collect();
    if prog.contains(&0) {
        return Err(Error::InteriorNul {
            what: "program path",
        });
    }
    if prog.contains(&QUOTE) {
        return Err(Error::UnsupportedExecutable {
            path: program.to_path_buf(),
        });
    }
    // argv[0] is parsed with simpler rules (no backslash escaping), so plain quoting is exact.
    out.push(QUOTE);
    out.extend_from_slice(&prog);
    out.push(QUOTE);
    for arg in args {
        out.push(SPACE);
        append_arg(&mut out, arg)?;
    }
    out.push(0);
    Ok(out)
}

fn append_arg(out: &mut Vec<u16>, arg: &OsStr) -> Result<()> {
    let arg: Vec<u16> = arg.encode_wide().collect();
    if arg.contains(&0) {
        return Err(Error::InteriorNul { what: "argument" });
    }
    let needs_quotes = arg.is_empty()
        || arg
            .iter()
            .any(|&c| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x22));
    if !needs_quotes {
        out.extend_from_slice(&arg);
        return Ok(());
    }
    out.push(QUOTE);
    let mut backslashes = 0usize;
    for &c in &arg {
        if c == BACKSLASH {
            backslashes += 1;
            continue;
        }
        if c == QUOTE {
            // 2n+1 backslashes followed by a quote yield n backslashes and a literal quote.
            out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
        }
        backslashes = 0;
        out.push(c);
    }
    // Trailing backslashes precede the closing quote, so they must be doubled.
    out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    out.push(QUOTE);
    Ok(())
}

/// Case-insensitive comparison of environment variable names, matching how Windows orders and
/// deduplicates them (ordinal, upper-cased UTF-16).
pub(crate) fn env_key_cmp(a: &OsStr, b: &OsStr) -> Ordering {
    let upper = |s: &OsStr| -> Vec<u16> {
        s.encode_wide()
            .map(|c| match c {
                0x61..=0x7A => c - 0x20,
                _ => c,
            })
            .collect()
    };
    upper(a).cmp(&upper(b))
}

/// A case-insensitive environment map with Windows semantics.
#[derive(Debug, Clone, Default)]
pub(crate) struct EnvMap {
    entries: Vec<(OsString, OsString)>,
}

impl EnvMap {
    pub(crate) fn from_current() -> Self {
        let mut map = EnvMap::default();
        for (k, v) in std::env::vars_os() {
            map.set(k, v);
        }
        map
    }

    pub(crate) fn get(&self, key: &OsStr) -> Option<&OsStr> {
        self.find(key).ok().map(|i| self.entries[i].1.as_os_str())
    }

    pub(crate) fn set(&mut self, key: OsString, value: OsString) {
        match self.find(&key) {
            Ok(i) => self.entries[i] = (key, value),
            Err(i) => self.entries.insert(i, (key, value)),
        }
    }

    pub(crate) fn remove(&mut self, key: &OsStr) {
        if let Ok(i) = self.find(key) {
            self.entries.remove(i);
        }
    }

    fn find(&self, key: &OsStr) -> std::result::Result<usize, usize> {
        self.entries.binary_search_by(|(k, _)| env_key_cmp(k, key))
    }

    /// Serializes into a sorted, double-NUL-terminated `CREATE_UNICODE_ENVIRONMENT` block.
    pub(crate) fn to_block(&self) -> Result<Vec<u16>> {
        let mut block = Vec::with_capacity(4096);
        for (k, v) in &self.entries {
            let start = block.len();
            block.extend(k.encode_wide());
            if block[start..].contains(&0)
                || k.is_empty()
                || block[start + 1..].contains(&(b'=' as u16))
            {
                return Err(Error::InteriorNul {
                    what: "environment variable name",
                });
            }
            block.push(b'=' as u16);
            let vstart = block.len();
            block.extend(v.encode_wide());
            if block[vstart..].contains(&0) {
                return Err(Error::InteriorNul {
                    what: "environment variable value",
                });
            }
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cl(args: &[&str]) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut v = command_line(Path::new(r"C:\bin\x.exe"), &args).unwrap();
        v.pop();
        String::from_utf16(&v).unwrap()
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(
            cl(&["plain", "a b", ""]),
            r#""C:\bin\x.exe" plain "a b" """#
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(cl(&[r#"a"b"#]), r#""C:\bin\x.exe" "a\"b""#);
        assert_eq!(cl(&[r#"a\"b"#]), r#""C:\bin\x.exe" "a\\\"b""#);
        assert_eq!(
            cl(&[r"dir\ with space\"]),
            r#""C:\bin\x.exe" "dir\ with space\\""#
        );
        // Backslashes not followed by a quote are literal and need no escaping.
        assert_eq!(
            cl(&[r"\\server\share\"]),
            r#""C:\bin\x.exe" \\server\share\"#
        );
    }

    #[test]
    fn rejects_interior_nul() {
        let args = vec![OsString::from("a\0b")];
        assert!(matches!(
            command_line(Path::new("x.exe"), &args),
            Err(Error::InteriorNul { .. })
        ));
    }

    #[test]
    fn env_is_case_insensitive_and_sorted() {
        let mut env = EnvMap::default();
        env.set("Path".into(), "a".into());
        env.set("PATH".into(), "b".into());
        env.set("ZED".into(), "z".into());
        env.set("alpha".into(), "1".into());
        assert_eq!(env.get(OsStr::new("path")), Some(OsStr::new("b")));
        let block = String::from_utf16(&env.to_block().unwrap()).unwrap();
        assert_eq!(block, "alpha=1\0PATH=b\0ZED=z\0\0");
        env.remove(OsStr::new("pAtH"));
        assert_eq!(env.get(OsStr::new("PATH")), None);
    }

    #[test]
    fn empty_env_block_is_double_nul() {
        assert_eq!(EnvMap::default().to_block().unwrap(), vec![0, 0]);
    }
}
