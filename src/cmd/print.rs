use std::borrow::Cow;

pub(super) fn stdout_path_bytes(path: &[u8]) -> Cow<'_, [u8]> {
    if !path.iter().any(|&b| needs_quoting(b)) {
        return Cow::Borrowed(path);
    }

    let mut out = Vec::with_capacity(path.len() + 2);
    out.push(b'"');
    for &b in path {
        match b {
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            // printable ASCII stays as is
            0x20..=0x7e => out.push(b),
            // everything else remaining controls (incl. ESC 0x1b), DEL, all bytes >= 0x80 become
            // 3-digit octal
            _ => {
                out.push(b'\\');
                out.push(b'0' + ((b >> 6) & 0o7));
                out.push(b'0' + ((b >> 3) & 0o7));
                out.push(b'0' + (b & 0o7));
            }
        }
    }
    out.push(b'"');
    Cow::Owned(out)
}

fn needs_quoting(b: u8) -> bool {
    b.is_ascii_control() // includes 0x7f
        || b >= 0x80 // non-ASCII, valid UTF-8 or not
        || b == b'"'
        || b == b'\\'
}