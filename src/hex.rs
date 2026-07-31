#[derive(Debug, PartialEq, Eq)]
pub(super) struct HexError {
    pub(super) digit: u8,
    pub(super) pos: usize,
}

pub(super) fn bytes_as_hex(bytes: &[u8; 20]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub(super) fn parse_hex(bytes: &[u8; 40]) -> Result<String, HexError> {
    for (index, &byte) in bytes.iter().enumerate() {
        if !is_hex_digit(byte) {
            return Err(HexError {
                digit: byte,
                pos: index,
            });
        }
    }
    // SAFETY: the previous loop guarantees that all bytes are ASCII
    Ok(unsafe { String::from_utf8_unchecked(bytes.to_vec()) })
}

pub(super) fn pair_to_u8(buf: &[u8; 2]) -> Result<u8, HexError> {
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
    if matches!(byte, b'0'..=b'9') {
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
