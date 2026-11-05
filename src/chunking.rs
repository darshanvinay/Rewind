//! Bounded-memory FastCDC 2020 chunking parameters.
use crate::{Error, Result};
use fastcdc::v2020;
use serde::{Deserialize, Serialize};

/// FastCDC sizes in bytes; validated before constructing a chunker.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkSizes {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for ChunkSizes {
    fn default() -> Self {
        Self {
            min: 16 * 1024,
            avg: 64 * 1024,
            max: 256 * 1024,
        }
    }
}

impl ChunkSizes {
    /// Check crate bounds, even sizes, ordering, and the memory safety cap.
    pub fn validate(self) -> Result<()> {
        if !(v2020::MINIMUM_MIN..=v2020::MINIMUM_MAX).contains(&(self.min as usize))
            || !(v2020::AVERAGE_MIN..=v2020::AVERAGE_MAX).contains(&(self.avg as usize))
            || !(v2020::MAXIMUM_MIN..=v2020::MAXIMUM_MAX).contains(&(self.max as usize))
            || !self.min.is_multiple_of(2)
            || !self.avg.is_multiple_of(2)
            || !self.max.is_multiple_of(2)
            || self.min > self.avg
            || self.avg > self.max
            || self.max > 16 * 1024 * 1024
        {
            return Err(Error::Invalid(
                "invalid chunk sizes (even sizes, min <= avg <= max, max <= 16 MiB)".into(),
            ));
        }
        Ok(())
    }
}
