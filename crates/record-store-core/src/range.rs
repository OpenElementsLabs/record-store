use std::{
    fmt::{self, Display, Formatter},
    num::{NonZeroU16, NonZeroU64},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::*;

/// A requested byte range expressed as an offset and non-zero length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    offset: u64,
    length: NonZeroU64,
}

impl ByteRange {
    /// Creates a range and rejects zero lengths or integer overflow.
    pub fn new(offset: u64, length: u64) -> Result<Self, CoreError> {
        let length = NonZeroU64::new(length)
            .ok_or_else(|| CoreError::InvalidByteRange("length must be non-zero".into()))?;
        offset
            .checked_add(length.get())
            .ok_or_else(|| CoreError::InvalidByteRange("offset plus length exceeds u64".into()))?;
        Ok(Self { offset, length })
    }

    /// Returns the first requested byte offset.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Returns the requested length.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.length.get()
    }

    /// Resolves the range against an object, truncating its tail at EOF.
    pub fn resolve(self, object_size: u64) -> Result<ResolvedByteRange, CoreError> {
        if self.offset >= object_size {
            return Err(CoreError::InvalidByteRange(
                "range starts at or beyond the end of the object".into(),
            ));
        }
        let available = object_size - self.offset;
        Ok(ResolvedByteRange {
            offset: self.offset,
            length: self.length().min(available),
        })
    }
}

/// A byte range resolved against a concrete object size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedByteRange {
    /// First returned byte offset.
    pub offset: u64,
    /// Number of bytes returned.
    pub length: u64,
}

/// S3 multipart part numbers are one-based and capped at 10,000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct PartNumber(NonZeroU16);

impl PartNumber {
    /// Highest part number accepted by S3 multipart uploads.
    pub const MAX: u16 = 10_000;

    /// Creates a validated part number.
    pub fn new(value: u16) -> Result<Self, CoreError> {
        if value > Self::MAX {
            return Err(CoreError::InvalidPartNumber(format!(
                "part number must be between 1 and {}",
                Self::MAX
            )));
        }
        NonZeroU16::new(value).map(Self).ok_or_else(|| {
            CoreError::InvalidPartNumber(format!("part number must be between 1 and {}", Self::MAX))
        })
    }

    /// Returns the one-based numeric part number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl TryFrom<u16> for PartNumber {
    type Error = CoreError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PartNumber> for u16 {
    fn from(value: PartNumber) -> Self {
        value.get()
    }
}

impl FromStr for PartNumber {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .map_err(|_| CoreError::InvalidPartNumber("part number must be an integer".into()))
            .and_then(Self::new)
    }
}

impl Display for PartNumber {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.get(), formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_ranges_are_checked_and_clamped() {
        assert!(ByteRange::new(0, 0).is_err());
        assert!(ByteRange::new(u64::MAX, 2).is_err());
        assert!(ByteRange::new(10, 1).expect("range").resolve(10).is_err());
        assert_eq!(
            ByteRange::new(5, 20)
                .expect("range")
                .resolve(10)
                .expect("resolved"),
            ResolvedByteRange {
                offset: 5,
                length: 5
            }
        );
    }
}
