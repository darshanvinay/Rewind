//! Canonical BLAKE3 identities. Validation prevents paths entering the store.
use crate::{Error, Result};

/// Hash bytes with the full BLAKE3 digest.
pub fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Reject noncanonical identities before filesystem path construction.
pub fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid(
            "expected a lowercase 64-digit BLAKE3 hash".into(),
        ));
    }
    Ok(())
}
