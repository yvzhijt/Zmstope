//! Small helpers shared by the app shell and the terminal bridge.

use std::path::{Path, PathBuf};

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64. Single pass, one allocation.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let t = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[((t >> 18) & 0x3F) as usize] as char);
        out.push(BASE64[((t >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64[((t >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[(t & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Escape a Rust string so it can be embedded in a single quoted JS string
/// literal. Every character JS forbids raw inside a literal is covered: the
/// quote, the backslash, the line terminators and the two "hard" separators
/// U+2028/U+2029.
pub fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8 + 16);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// Turn a path or command into something worth showing in a window title.
pub fn shell_label(program: &str) -> &str {
    let stem = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    if stem.is_empty() {
        program
    } else {
        stem
    }
}

/// Drop Windows' verbatim prefix, so a path can be shown or handed to a shell.
///
/// `std::fs::canonicalize` returns `\\?\D:\src\app` on Windows — the extended
/// length form that lifts the 260-character `MAX_PATH` limit. Two things go
/// wrong once it escapes into the UI:
///
/// * It is a storage detail nobody asked to read, and on screen it is worse
///   than ugly: `\` and `?` are bidi-neutral, so the sidebar's RTL truncation
///   reverses it into a trailing `\?\\` and the header reads as garbage.
/// * `CreateProcess`'s current directory and PowerShell's `Set-Location` both
///   expect a plain path, so the terminal either starts somewhere else or
///   prints the prefix in every prompt.
///
/// Stripping at the boundary keeps `MAX_PATH`-breaking paths working for
/// ordinary file I/O while the places a person or a shell looks stay clean.
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    // `\\?\UNC\server\share\dir` is `\\server\share\dir`, not a bare UNC body.
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::strip_verbatim;
    use std::path::Path;

    #[test]
    fn drops_the_verbatim_prefix() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\D:\xm\mapp\desktop\a")),
            Path::new(r"D:\xm\mapp\desktop\a")
        );
    }

    #[test]
    fn turns_a_verbatim_unc_path_back_into_a_unc_path() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\fileserver\share\proj")),
            Path::new(r"\\fileserver\share\proj")
        );
    }

    #[test]
    fn leaves_ordinary_paths_alone() {
        assert_eq!(
            strip_verbatim(Path::new(r"D:\xm\mapp")),
            Path::new(r"D:\xm\mapp")
        );
        assert_eq!(
            strip_verbatim(Path::new("/home/me/proj")),
            Path::new("/home/me/proj")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\srv\share")),
            Path::new(r"\\srv\share")
        );
    }
}
