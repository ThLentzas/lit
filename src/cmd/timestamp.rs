use chrono::Local;

// toDo: fow now the date for commit is generated at every new commit. In the future when we add
// support for Date formats we will adjust the logic.
// https://git-scm.com/docs/git-commit#_date_formats
pub(super) fn now() -> String {
    let now = Local::now();
    format!("{} {}", now.timestamp(), now.format("%z"))
}

// In RFC 2822-style dates, it is the rule that lets whitespace be written either as normal
// spaces/tabs, or as a line break followed by spaces/tabs.
//
// FWS = ([*WSP CRLF] 1*WSP) / obs-FWS it reads as: optional spaces/tabs, then a line break, then at
// least one space/tab or at least one space/tab. A date like Thu, 07 Apr 2005 22:13:13 +0200 can
// be folded to:
//      Thu, 07 Apr 2005
//       22:13:13 +0200
//
// The parser treats the CRLF followed by whitespace as if it were just a space. It exists because
// RFC 2822 is an email-message format and Email headers can be long, so they can be “folded” across 
// multiple physical lines.
fn normalize_fws(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut i = 0;

    while i < bytes.len() {
        if i + 2 < bytes.len()
            && bytes[i] == b'\r'
            && bytes[i + 1] == b'\n'
            && is_wsp(bytes[i + 2])
        {
            // CRLF followed by WSP is folded whitespace.
            // Replace the whole folded whitespace run with one space.
            out.push(' ');
            i += 2;
            while i < bytes.len() && is_wsp(bytes[i]) {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn is_wsp(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}