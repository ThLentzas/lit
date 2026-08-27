use std::borrow::Cow;
use std::{fmt, io};

pub(crate) struct ReadableByte(pub(crate) u8);

impl fmt::Display for ReadableByte {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // space is not considered graphic in this case, because unexpected byte  looks ambiguous
        if self.0.is_ascii_graphic() {
            write!(f, "{}", self.0 as char)
        } else {
            write!(f, "0x{:02x}", self.0)
        }
    }
}


// we could also define it as Printer<T>, it is classic associated types vs generics in traits
pub(super) trait Printer {
    // the type of value we need to print, it is an associated type because similar to Iterators we
    // are only going to print one T
    //
    // I think associated type is still the right choice, because semantically a StatusPrinter prints
    // a Report, a ConfigPrinter prints ConfigEntries, a CatFilePrinter prints an Object. Only one
    // T at each case. If we go the generic way, nothing stops us from implementing ConfigPrinter
    // for more than one T.
    //
    // The lifetime here is needed because in some cases T can hold a reference. Such case is a
    // ConfigEntry that its value holds Cow<'_, [u8]>. If the value needs no special handling
    // the value can borrow directly from the file buffer avoiding unnecessary allocations.
    // For owned types doing type T<'a> = Report means that no matter the lifetime T does not change.
    // It is not affected at all. The only downside is syntactic because every printer impl must
    // declare T<'a> even when it does not need 'a.
    //
    // <'short> = Report,
    // <'long> = Report,
    // <'static> = Report,
    //
    // type T: 'a means that T does not contain any reference that expires before a
    type T<'a>;
    
    fn print<'a>(&self, value: &Self::T<'a>,) -> io::Result<()>;
}

// TODO: this is also C-quoted representation, should we rename?
pub(super) fn stdout_path_bytes(path: &[u8]) -> Cow<'_, [u8]> {
    if !path.iter().any(|&b| needs_quoting(b)) {
        // this is safe, all characters that needs special handling are in the is_ascii_control()
        // range check we do in needs_quoting()
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