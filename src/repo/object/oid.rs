use crate::repo::object::{pair_to_u8, OidError, pair_to_u8_unchecked};

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