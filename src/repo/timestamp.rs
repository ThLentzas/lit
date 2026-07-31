use chrono::{Local, Offset};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TimestampError {
    BadUnixTimeStamp,
    BadTimezone,
}

pub(super) struct Timestamp {
    // i64 because a Unix timestamp is an offset in seconds from the Unix epoch: 1970-01-01 00:00:00 UTC
    // a negative value means before epoch
    pub(super) unix: i64,
    pub(super) timezone: [u8; 5],
}

impl Timestamp {
    // toDo: fow now the date for commit is generated at every new commit. In the future when we add
    // support for Date formats we will adjust the logic.
    // https://git-scm.com/docs/git-commit#_date_formats
    pub(super) fn now() -> Self {
        let now = Local::now();
        let secs = now.offset().fix().local_minus_utc();
        let sign = if secs < 0 { b'-' } else { b'+' };
        let total_mins = secs.abs() / 60;
        let hours = total_mins / 60;
        let mins = hours / 60;

        let timezone = [
            sign,
            b'0' + ((hours / 10) as u8),
            b'0' + ((hours % 10) as u8),
            b'0' + ((mins / 10) as u8),
            b'0' + ((mins % 10) as u8),
        ];

        Self {
            unix: now.timestamp(),
            timezone,
        }
    }

    pub(super) fn from_bytes(unix: &[u8], timezone: &[u8]) -> Result<Self, TimestampError> {
        let (neg, digits) = match unix.split_first() {
            Some((b'-', rest)) => (true, rest),
            _ => (false, unix),
        };
        if digits.is_empty() {
            return Err(TimestampError::BadUnixTimeStamp);
        }
        let mut n: i64 = 0;
        for &b in digits {
            let d = (b as char)
                .to_digit(10)
                .ok_or(TimestampError::BadUnixTimeStamp)? as i64;
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(d))
                .ok_or(TimestampError::BadUnixTimeStamp)?;
        }
        let unix = if neg { -n } else { n };

        let timezone: [u8; 5] = timezone.try_into().unwrap();
        if (timezone[0] == b'-' || timezone[0] == b'+')
            && timezone[1..].iter().all(u8::is_ascii_digit)
        {
            // always safe because we already checked that timezone contains only ASCII digits
            let hours = (timezone[1] as char)
                .to_digit(10)
                .and_then(|n| n.checked_mul(10))
                .and_then(|n| n.checked_add((timezone[2] as char).to_digit(10).unwrap()))
                .unwrap();
            let minutes = (timezone[3] as char)
                .to_digit(10)
                .and_then(|n| n.checked_mul(10))
                .and_then(|n| n.checked_add((timezone[4] as char).to_digit(10).unwrap()))
                // we could also do: .ok_or(TimestampError::BadUnixTimeStamp)?;
                .unwrap();
            // -2359..2359
            if hours > 23 || minutes > 59 {
                return Err(TimestampError::BadUnixTimeStamp);
            }
            Ok(Timestamp { unix, timezone })
        } else {
            Err(TimestampError::BadTimezone)
        }
    }

    pub(super) fn to_string(&self) -> String {
        // SAFETY: the [u8; 5] is always ASCII
        let timezone = unsafe { String::from_utf8_unchecked(self.timezone.to_vec()) };
        format!("{} {}", self.unix.to_string(), timezone)
    }
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
            && is_whitespace(bytes[i + 2])
        {
            // CRLF followed by WSP is folded whitespace.
            // Replace the whole folded whitespace run with one space.
            out.push(' ');
            i += 2;
            while i < bytes.len() && is_whitespace(bytes[i]) {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}
