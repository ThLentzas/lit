use std::{error, fmt};

const UTF8_CHAR_WIDTH: [u8; 256] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    4, 4, 4, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

pub(super) fn check_sequence(buffer: &[u8], pos: usize) -> Result<(), Utf8Error> {
    let mut i = pos;
    let first = buffer[i];
    let width = UTF8_CHAR_WIDTH[first as usize];

    match width {
        2 => {
            let Some(second) = next(buffer, &mut i) else {
                return Err(Utf8Error { len: 2, pos });
            };
            if second as i8 >= -64 {
                return Err(Utf8Error { len: 2, pos });
            }
        }
        3 => {
            let Some(second) = next(buffer, &mut i) else {
                return Err(Utf8Error { len: 3, pos });
            };
            match (first, second) {
                (0xE0, 0xA0..=0xBF)
                | (0xE1..=0xEC, 0x80..=0xBF)
                | (0xED, 0x80..=0x9F)
                | (0xEE..=0xEF, 0x80..=0xBF) => {}
                _ => return Err(Utf8Error { len: 3, pos }),
            }
            let Some(third) = next(buffer, &mut i) else {
                return Err(Utf8Error { len: 3, pos });
            };
            if third as i8 >= -64 {
                return Err(Utf8Error { len: 3, pos });
            }
        }
        4 => {
            let Some(second) = next(buffer, &mut i) else {
                return Err(Utf8Error { len: 4, pos });
            };
            match (first, second) {
                (0xF0, 0x90..=0xBF) | (0xF1..=0xF3, 0x80..=0xBF) | (0xF4, 0x80..=0x8F) => {}
                _ => return Err(Utf8Error { len: 4, pos }),
            }
            for _ in 0..2 {
                let Some(next) = next(buffer, &mut i) else {
                    return Err(Utf8Error { len: 4, pos });
                };
                if next as i8 >= -64 {
                    return Err(Utf8Error { len: 4, pos });
                }
            }
        }
        _ => return Err(Utf8Error { len: 1, pos }),
    }
    Ok(())
}

pub(super) fn char_width(b: u8) -> usize {
    UTF8_CHAR_WIDTH[b as usize] as usize
}

pub(super) fn is_bom_present(buffer: &[u8]) -> bool {
    buffer.len() >= 3 && (buffer[0], buffer[1], buffer[2]) == (0xEF, 0xBB, 0xBF)
}

pub(super) fn read_char(buffer: &[u8], pos: usize) -> char {
    let width = char_width(buffer[pos]);
    // SAFETY: always called on a valid sequence
    unsafe {
        str::from_utf8_unchecked(&buffer[pos..pos + width])
            .chars()
            .next()
            .unwrap()
    }
}

// only 1 kind of Utf8Error, InvalidByteSequence
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Utf8Error {
    pub(super) len: u8,
    pub(super) pos: usize,
}

impl error::Error for Utf8Error {}

impl fmt::Display for Utf8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {} byte utf-8 sequence from index {}",
            self.len, self.pos
        )
    }
}

fn next(bytes: &[u8], pos: &mut usize) -> Option<u8> {
    *pos += 1;
    if *pos >= bytes.len() {
        return None;
    }
    Some(bytes[*pos])
}