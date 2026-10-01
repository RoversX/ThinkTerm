// Vendored: see Cargo.toml for decoder resource limits.
#![allow(bare_trait_objects, deprecated, unused_attributes, unused_mut, dropping_references)]
//! varbincode is a binary serialization format that uses variable
//! length encoding for integer values, which typically results in
//! reduced size of the encoded data.
pub mod de;
pub mod error;
pub mod ser;
#[cfg(test)]
mod test;

pub use de::Deserializer;
pub use ser::Serializer;

/// Growth to this size or past it moves to a fresh allocation; see
/// `reserve`.
pub const LARGE_GROWTH: usize = 1024 * 1024;

/// Make room in `buf` for `additional` more bytes, growing as `Vec` would
/// (doubling, at least 8) but never past `limit`, which callers that know
/// the final size pass. Growth to `LARGE_GROWTH` or more moves the bytes to
/// a fresh allocation instead of `realloc` extending the block: macOS keeps
/// the large blocks such a `realloc` leaves behind rather than reusing
/// them, and picture frames decoded that way held over a gigabyte of freed
/// memory in each process. Doubling still means a buffer is copied about as
/// many bytes as it ends up holding.
pub fn reserve(buf: &mut Vec<u8>, additional: usize, limit: usize) {
    let needed = buf.len().saturating_add(additional);
    if needed <= buf.capacity() {
        return;
    }
    let capacity = buf
        .capacity()
        .saturating_mul(2)
        .max(needed)
        .max(8)
        .min(limit.max(needed));
    if capacity < LARGE_GROWTH {
        buf.reserve_exact(capacity - buf.len());
        return;
    }
    let mut grown = Vec::with_capacity(capacity);
    grown.extend_from_slice(buf);
    *buf = grown;
}

/// A convenience function for serializing a value as a byte vector
/// See also `ser::Serializer`.
pub fn serialize<T: serde::Serialize>(t: &T) -> Result<Vec<u8>, error::Error> {
    let mut result = Vec::new();
    let mut s = Serializer::new(&mut result);
    t.serialize(&mut s)?;
    Ok(result)
}

/// A convenience function for deserializing from a stream.
/// See also `de::Deserializer`.
pub fn deserialize<T: serde::de::DeserializeOwned, R: std::io::Read>(
    mut r: R,
) -> Result<T, error::Error> {
    let mut d = Deserializer::new(&mut r);
    serde::Deserialize::deserialize(&mut d)
}
