use crate::repo::object::{HexError, OidError};
use std::fmt;

// the hex representation is used as a filename and since any byte is valid for the [u8; 20] hash
// how do we know we don't get NUL bytes when we call as_bytes()? Because the actual bytes we get
// are for hex representation, the string itself, so even if '0' exists we never get 0 but 48
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct Oid {
    inner: [u8; 20],
}

impl Oid {
    pub(crate) fn from_bytes(bytes: [u8; 20]) -> Self {
        Self { inner: bytes }
    }

    pub(crate) fn from_hex_bytes(hex: &[u8]) -> Result<Self, OidError> {
        if hex.len() != 40 {
            return Err(OidError::BadLength);
        }
        // we can try with MaybeUni
        let mut inner = [0u8; 20];

        for (i, pair) in hex.chunks_exact(2).enumerate() {
            inner[i] = pair_to_u8(pair.try_into().unwrap()).map_err(|err| OidError::BadDigit {
                pos: i + err.pos,
                digit: err.digit,
            })?;
        }
        Ok(Self { inner })
    }

    pub(crate) fn from_hex_bytes_unchecked(hex: &[u8]) -> Self {
        let mut inner = [0u8; 20];

        for (i, pair) in hex.chunks_exact(2).enumerate() {
            inner[i] = unsafe { pair_to_u8_unchecked(pair.try_into().unwrap()) };
        }

        Self { inner }
    }

    pub(crate) fn from_hex(hex: &str) -> Result<Self, OidError> {
        let hex = hex.to_lowercase();
        let bytes = hex.as_bytes();
        Self::from_hex_bytes(bytes)
    }

    // methods to_* that impl Copy take self
    pub(crate) fn to_hex(self) -> String {
        self.inner.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 20] {
        &self.inner
    }
    // TODO: const and _inner()
    pub(crate) fn inner(&self) -> [u8; 20] {
        self.inner
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

fn pair_to_u8_unchecked(buf: &[u8; 2]) -> u8 {
    let first = to_base10_digit(buf[0]);
    let second = to_base10_digit(buf[1]);
    (first << 4) | second
}

fn pair_to_u8(buf: &[u8; 2]) -> Result<u8, HexError> {
    let first = buf[0];
    let second = buf[1];

    if !is_hex_digit(first) {
        return Err(HexError {
            digit: first,
            pos: 0,
        });
    }
    if !is_hex_digit(second) {
        return Err(HexError {
            digit: second,
            pos: 1,
        });
    }

    let first = to_base10_digit(first);
    let second = to_base10_digit(second);
    // there are a lot of ways to write the conversion
    // This is what we want: second * 16u8.pow(0) + first * 16u8.pow(1) but because 16^0 is always 0
    // and 16^1 is always 16 we can write as follows first * 16 + second
    //
    // 1 byte = [4 high] [4 bits]
    // because each hex digit is in the 0 - 15 range we can use exactly 4 bits
    // 'af' -> 'a' = 10 = 1010, 'f' = 15 = 1111, 10101111
    //
    // 1011 are the high bits 1111 are the low bits
    // first << 4 moves first into the high bits and the low bits of the number are all 0s
    // 'a' as u8 is written as 00001011 with extra padding, shifting 10110000
    // next we want to set 'f' to the low bits, we use OR
    // a OR 0 = a
    // 'f' in u8 is 00001111 so the high bits of 'a' are ORed with 0 so they stay as is and the low
    // bits of 'a' are 0s which are ORed with the low bits of 'f' and become 'f'
    Ok((first << 4) | second)
}

fn to_base10_digit(byte: u8) -> u8 {
    if byte.is_ascii_digit() {
        byte - b'0'
    } else {
        byte - b'a' + 10
    }
}

// we can't use the is_ascii_hex() from std because it includes the capital case letters and Git
// writes the hash always using lower case letters. Even if they are same in some sense, we have to
// stay case-sensitive because they produce different hashes when it comes to storing commits.
fn is_hex_digit(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}
