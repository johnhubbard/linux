// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::prelude::*;

use super::types::{
    Index,
    KeyId,
    Op,
    Opcode, //
};

/// An encoder for an NVKV stream.
pub(crate) struct Encoder {
    backing: KVVec<u64>,
}

impl Encoder {
    /// Creates an empty encoder.
    pub(crate) fn new() -> Self {
        Self {
            backing: KVVec::new(),
        }
    }

    /// Appends `bytes` to the stream, padded to a multiple of 8 bytes.
    fn push_bytes_with_padding(&mut self, bytes: &[u8]) -> Result {
        let num_entries = bytes.len().div_ceil(size_of::<u64>());
        self.backing.reserve(num_entries, GFP_KERNEL)?;

        let spare = self.backing.spare_capacity_mut();
        let dst = spare.as_mut_ptr().cast::<u8>();

        // SAFETY: We are guaranteed at least `bytes.len()` bytes of space since we just reserved
        // `num_entries` worth of space.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len()) };

        let padding = num_entries * size_of::<u64>() - bytes.len();
        if padding > 0 {
            // SAFETY: We are guaranteed at least `num_entries * size_of::<u64>()` bytes of space.
            unsafe { core::ptr::write_bytes(dst.add(bytes.len()), 0, padding) };
        }

        // SAFETY: We just initialized these bytes and every bit pattern is valid for `u64`.
        unsafe { self.backing.inc_len(num_entries) };

        Ok(())
    }

    /// Returns the encoded data.
    #[must_use = "encoded data must be consumed"]
    pub(crate) fn finish(self) -> KVVec<u64> {
        self.backing
    }

    #[inline(always)]
    fn encode_op(&mut self, op: Op) -> Result {
        self.backing.push(op.into_raw(), GFP_KERNEL)?;
        Ok(())
    }

    /// Encodes a 32-bit value as an IMM32 pair, with the value in the op word.
    #[inline(always)]
    pub(crate) fn encode_u32(&mut self, key: KeyId, index: Index, value: u32) -> Result {
        // TODO: Consider automatically merging sequential keys.
        self.encode_op(
            Op::zeroed()
                .with_key(key)
                .with_index(index)
                .with_opcode(Opcode::Imm32)
                .with_value(value),
        )?;
        Ok(())
    }

    /// Encodes a 64-bit value as a single-element SEQ64 pair.
    #[inline(always)]
    pub(crate) fn encode_u64(&mut self, key: KeyId, index: Index, value: u64) -> Result {
        // TODO: Consider automatically merging sequential keys.
        const KEY_COUNT: u32 = 1;
        self.backing.reserve(2, GFP_KERNEL)?;
        self.encode_op(
            Op::zeroed()
                .with_key(key)
                .with_index(index)
                .with_opcode(Opcode::Seq64)
                .with_value(KEY_COUNT),
        )?;
        self.backing.push_within_capacity(value)?;
        Ok(())
    }

    /// Encodes a byte array as an ARRAY8 pair, zero-padded to a multiple of 8 bytes.
    #[inline(always)]
    pub(crate) fn encode_array8(&mut self, key: KeyId, index: Index, array: &[u8]) -> Result {
        let value_count = u32::try_from(array.len())?;
        let num_entries = array.len().div_ceil(size_of::<u64>());
        self.backing.reserve(num_entries + 1, GFP_KERNEL)?;
        self.encode_op(
            Op::zeroed()
                .with_key(key)
                .with_index(index)
                .with_opcode(Opcode::Array8)
                .with_value(value_count),
        )?;
        self.push_bytes_with_padding(array.as_bytes())?;
        Ok(())
    }

    /// Encodes a 32-bit array as an ARRAY32 pair, zero-padded to a multiple of 8 bytes.
    #[inline(always)]
    pub(crate) fn encode_array32(&mut self, key: KeyId, index: Index, array: &[u32]) -> Result {
        let value_count = u32::try_from(array.len())?;
        let num_entries = array.len().div_ceil(2);
        self.backing.reserve(num_entries + 1, GFP_KERNEL)?;
        self.encode_op(
            Op::zeroed()
                .with_key(key)
                .with_index(index)
                .with_opcode(Opcode::Array32)
                .with_value(value_count),
        )?;
        self.push_bytes_with_padding(array.as_bytes())?;
        Ok(())
    }

    /// Encodes a 64-bit array as an ARRAY64 pair.
    #[inline(always)]
    pub(crate) fn encode_array64(&mut self, key: KeyId, index: Index, array: &[u64]) -> Result {
        let value_count = u32::try_from(array.len())?;
        self.backing.reserve(array.len() + 1, GFP_KERNEL)?;
        self.encode_op(
            Op::zeroed()
                .with_key(key)
                .with_index(index)
                .with_opcode(Opcode::Array64)
                .with_value(value_count),
        )?;
        self.push_bytes_with_padding(array.as_bytes())?;
        Ok(())
    }
}
