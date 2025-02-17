// SPDX-License-Identifier: GPL-2.0

//! Numerical and binary utilities for primitive types.

/// Useful operations for `u64`.
pub trait U64Ext {
    /// Build a `u64` by combining its `high` and `low` parts.
    ///
    /// ```
    /// use kernel::num::U64Ext;
    /// assert_eq!(u64::from_u32s(0x01234567, 0x89abcdef), 0x01234567_89abcdef);
    /// ```
    fn from_u32s(high: u32, low: u32) -> Self;

    /// Returns the `(high, low)` u32s that constitute `self`.
    ///
    /// ```
    /// use kernel::num::U64Ext;
    /// assert_eq!(u64::into_u32s(0x01234567_89abcdef), (0x1234567, 0x89abcdef));
    /// ```
    fn into_u32s(self) -> (u32, u32);
}

impl U64Ext for u64 {
    fn from_u32s(high: u32, low: u32) -> Self {
        ((high as u64) << u32::BITS) | low as u64
    }

    fn into_u32s(self) -> (u32, u32) {
        ((self >> u32::BITS) as u32, self as u32)
    }
}
