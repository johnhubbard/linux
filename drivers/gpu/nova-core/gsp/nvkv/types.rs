// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use kernel::{
    bitfield,
    num::Bounded,
    prelude::*, //
};

/// The identifier of an NVKV key.
pub(crate) type KeyId = u16;

/// The index of an NVKV value.
pub(crate) type Index = Bounded<u64, 12>;

/// A value that encodes and decodes under the NVKV key `KEY_ID`.
///
/// The value has type `T`. The wire representation for encoding is `As`.
#[repr(transparent)]
pub(crate) struct Key<T, const KEY_ID: KeyId, As = T>(pub(crate) T, PhantomData<As>);

impl<T, const KEY_ID: KeyId, As> From<T> for Key<T, KEY_ID, As> {
    fn from(value: T) -> Self {
        Self(value, PhantomData)
    }
}

impl<'a, T, const KEY_ID: KeyId, As, const N: usize> From<&'a [T; N]> for Key<&'a [T], KEY_ID, As> {
    fn from(value: &'a [T; N]) -> Self {
        Self(&value[..], PhantomData)
    }
}

impl<T, const KEY_ID: KeyId, As> Deref for Key<T, KEY_ID, As> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, const KEY_ID: KeyId, As> DerefMut for Key<T, KEY_ID, As> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T: Default, const KEY_ID: KeyId, As> Default for Key<T, KEY_ID, As> {
    fn default() -> Self {
        Self(T::default(), PhantomData)
    }
}

/// A fixed capacity vector that holds at most `N` elements.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Zeroable)]
pub(crate) struct ArrayVec<T, const N: usize> {
    data: [T; N],
    len: usize,
}

impl<T, const N: usize> ArrayVec<T, N> {
    /// Replaces the contents with a copy of `slice`.
    ///
    /// Fails with `EINVAL` if `slice` is longer than the capacity.
    pub(crate) fn set_from_slice(&mut self, slice: &[T]) -> Result
    where
        T: Copy,
    {
        let Some(dst) = self.data.get_mut(..slice.len()) else {
            return Err(EINVAL);
        };

        dst.copy_from_slice(slice);
        self.len = slice.len();

        Ok(())
    }

    /// Returns the initialized elements as a slice.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[T] {
        // PANIC: `len` is bounded by `N`; all constructors and mutators maintain this invariant.
        &self.data[..self.len]
    }
}

impl<T: Default + Copy, const N: usize> Default for ArrayVec<T, N> {
    fn default() -> Self {
        Self {
            data: [T::default(); N],
            len: 0,
        }
    }
}

impl<T, const N: usize> Deref for ArrayVec<T, N> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// A schema field for an array value under the NVKV key `KEY_ID`.
#[derive(Default)]
#[repr(transparent)]
pub(crate) struct Array<T: Default + Copy, const N: usize, const KEY_ID: KeyId>(
    pub(crate) ArrayVec<T, N>,
);

impl<T: Default + Copy, const N: usize, const KEY_ID: KeyId> Array<T, N, KEY_ID> {
    /// Creates an array filled from `values`.
    ///
    /// Fails with `EINVAL` if `values` is longer than `N`.
    pub(crate) fn new(values: &[T]) -> Result<Self> {
        let mut data = ArrayVec::default();
        data.set_from_slice(values)?;
        Ok(Self(data))
    }
}

bitfield! {
    /// The op word that starts each NVKV operation.
    pub(super) struct Op(u64) {
        15:0 key;
        27:16 index => Index;
        31:28 opcode ?=> Opcode;
        63:32 value;
    }
}

/// Describes the format of the following NVKV operation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Opcode {
    /// A 32-bit value in the op word.
    Imm32 = 0,
    /// 32-bit values for consecutive keys, starting at the pair's key.
    Seq32 = 1,
    /// 64-bit values for consecutive keys, starting at the pair's key.
    Seq64 = 2,
    /// An array of bytes.
    Array8 = 3,
    /// An array of 32-bit elements.
    Array32 = 4,
    /// An array of 64-bit elements.
    Array64 = 5,
}

// TODO[FPRI]: This is a temporary solution to be replaced with the corresponding derive macros once
// they land.
impl TryFrom<Bounded<u64, 4>> for Opcode {
    type Error = Error;

    fn try_from(value: Bounded<u64, 4>) -> Result<Self> {
        match value.get() {
            0 => Ok(Self::Imm32),
            1 => Ok(Self::Seq32),
            2 => Ok(Self::Seq64),
            3 => Ok(Self::Array8),
            4 => Ok(Self::Array32),
            5 => Ok(Self::Array64),
            _ => Err(EINVAL),
        }
    }
}

impl From<Opcode> for Bounded<u64, 4> {
    fn from(value: Opcode) -> Self {
        Bounded::from_expr(value as u64)
    }
}
