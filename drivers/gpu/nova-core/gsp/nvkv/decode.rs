// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use core::marker::PhantomData;

use kernel::prelude::*;

use crate::gsp::nvkv::{Array, ArrayVec, Index, Key, KeyId, Op, Opcode};
use crate::num;

/// Defines a schema struct together with its [`Schema`] implementation that decodes into
/// `$target`.
macro_rules! nvkv_decode {
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident => $target:ident {
            $(
                $(#[$field_attr:meta])*
                $field_vis:vis $field:ident : $ty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$attr])*
        $vis struct $name {
            $(
                $(#[$field_attr])*
                $field_vis $field: $ty,
            )*
        }

        impl $crate::gsp::nvkv::Schema for $name {
            type Target = $target;

            fn visit(
                &mut self,
                key: $crate::gsp::nvkv::KeyId,
                index: $crate::gsp::nvkv::Index,
                value: $crate::gsp::nvkv::DecoderValue<'_>,
            ) -> ::kernel::error::Result<bool> {
                Ok(false
                    $( || $crate::gsp::nvkv::Schema::visit(&mut self.$field, key, index, value)? )*)
            }

            #[inline(always)]
            fn finish(self) -> impl ::kernel::prelude::Init<Self::Target, ::kernel::error::Error> {
                ::kernel::try_init!(Self::Target {
                    $( $field <- $crate::gsp::nvkv::Schema::finish(self.$field), )*
                }? ::kernel::error::Error)
            }
        }
    };
}
pub(crate) use nvkv_decode;

impl<T: for<'a> TryFrom<DecoderValue<'a>, Error = Error> + Default, const KEY_ID: KeyId> Schema
    for Key<T, KEY_ID>
{
    type Target = T;

    #[inline(always)]
    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            Ok(false)
        } else if index != Index::new::<0>() {
            // Stability: Single values being set must be at index 0.
            Err(EINVAL)
        } else {
            // Stability: We overwrite and take the latest value here.
            self.0 = value.try_into()?;
            Ok(true)
        }
    }

    #[inline(always)]
    fn finish(self) -> impl Init<Self::Target, Error> {
        Ok(self.0)
    }
}

impl<T: for<'a> TryFrom<DecoderValue<'a>, Error = Error>, const KEY_ID: KeyId> Schema
    for Key<Option<T>, KEY_ID>
{
    type Target = Option<T>;

    #[inline(always)]
    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            Ok(false)
        } else if index != Index::new::<0>() {
            // Stability: Single values being set must be at index 0.
            Err(EINVAL)
        } else {
            // Stability: We overwrite and take the latest value here.
            self.0 = Some(value.try_into()?);
            Ok(true)
        }
    }

    #[inline(always)]
    fn finish(self) -> impl Init<Self::Target, Error> {
        Ok(self.0)
    }
}

impl<T: Default + Copy, const N: usize, const KEY_ID: KeyId> Schema for Array<T, N, KEY_ID>
where
    for<'a> &'a [T]: TryFrom<DecoderValue<'a>, Error = Error>,
{
    type Target = ArrayVec<T, N>;

    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            return Ok(false);
        }
        // Stability: Require to be at index 0
        if index != Index::new::<0>() {
            return Err(EINVAL);
        }
        // Stability: Reject oversized and take the latest value.
        self.0.set_from_slice(value.try_into()?)?;
        Ok(true)
    }

    #[inline(always)]
    fn finish(self) -> impl Init<Self::Target, Error> {
        Ok(self.0)
    }
}

/// A schema field for a key that must be present.
///
/// `finish` fails with `EINVAL` if no value arrived for the key.
#[repr(transparent)]
pub(crate) struct Required<T, const KEY_ID: KeyId>(Key<Option<T>, KEY_ID>);

impl<T: for<'a> TryFrom<DecoderValue<'a>, Error = Error>, const KEY_ID: KeyId> Schema
    for Required<T, KEY_ID>
{
    type Target = T;

    #[inline(always)]
    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        self.0.visit(key, index, value)
    }

    #[inline(always)]
    fn finish(self) -> impl Init<Self::Target, Error> {
        (self.0).0.ok_or(EINVAL)
    }
}

impl<T, const KEY_ID: KeyId> Default for Required<T, KEY_ID> {
    fn default() -> Self {
        Self(None.into())
    }
}

/// Expects objects specified sequentially with index starting from zero.
pub(crate) struct Accumulated<S: Schema> {
    current_index: Index,
    current: S,
    current_started: bool,
    next: S,
    accumulated: KVVec<S::Target>,
}

impl<S: Schema + Default> Accumulated<S> {
    /// Creates an empty accumulator.
    pub(crate) fn new() -> Self {
        Self {
            current_index: Index::new::<0>(),
            current: S::default(),
            current_started: false,
            next: S::default(),
            accumulated: KVVec::new(),
        }
    }

    fn into_vec(mut self) -> Result<KVVec<S::Target>> {
        if self.current_started {
            let done = core::mem::take(&mut self.current);
            self.accumulated.push_init(done.finish(), GFP_KERNEL)?;
        }
        Ok(self.accumulated)
    }
}

impl<S: Schema + Default> Schema for Accumulated<S> {
    type Target = KVVec<S::Target>;

    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if index != self.current_index {
            if !self.next.visit(key, Index::new::<0>(), value)? {
                // Unrelated key to us.
                return Ok(false);
            }

            // Stability: We require that objects at index k have all their keys sent before the k
            // + 1 th object can be completed. We require that objects are sent contiguously in
            // order from index 0.
            if !self.current_started || index != self.current_index + 1 {
                return Err(EINVAL);
            }

            // We must have finished the current value. Finish it and start working on `next`.
            let done = core::mem::replace(&mut self.current, core::mem::take(&mut self.next));
            self.accumulated.push_init(done.finish(), GFP_KERNEL)?;
            self.current_started = true;
            self.current_index = index;
            Ok(true)
        } else {
            let consumed = self.current.visit(key, Index::new::<0>(), value)?;
            self.current_started |= consumed;
            Ok(consumed)
        }
    }

    #[inline(always)]
    fn finish(self) -> impl Init<Self::Target, Error> {
        self.into_vec()
    }
}

impl<S: Schema + Default> Default for Accumulated<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// A schema field that scatters indexed values into an array of `N` slots.
#[repr(transparent)]
pub(crate) struct Indexed<T, const N: usize, const KEY_ID: KeyId, As = T>([T; N], PhantomData<As>);

/// Copies `elems`, converted to `T`, into `slots` at `start`.
///
/// Fails with `EINVAL` if the window does not fit in `slots`.
fn scatter_window<T: From<As>, As: Copy>(slots: &mut [T], start: usize, elems: &[As]) -> Result {
    let end = start.checked_add(elems.len()).ok_or(EINVAL)?;
    // Stability: We reject indices outside of the declared array size (maybe want to ignore them?)
    let dst = slots.get_mut(start..end).ok_or(EINVAL)?;
    for (d, &e) in dst.iter_mut().zip(elems) {
        *d = T::from(e);
    }
    Ok(())
}

impl<T, const N: usize, const KEY_ID: KeyId, As> Schema for Indexed<T, N, KEY_ID, As>
where
    T: From<As>,
    As: Copy + for<'a> TryFrom<DecoderValue<'a>, Error = Error>,
    for<'a> &'a [As]: TryFrom<DecoderValue<'a>, Error = Error>,
{
    type Target = [T; N];

    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            return Ok(false);
        }
        let start = index.cast::<usize>().get();
        // Stability: We accept both scalar vs scattered array setting for flexibility.
        match <&[As]>::try_from(value) {
            Ok(elems) => scatter_window(&mut self.0, start, elems)?,
            Err(_) => scatter_window(&mut self.0, start, &[As::try_from(value)?])?,
        }
        Ok(true)
    }

    #[inline(always)]
    fn finish(self) -> impl Init<Self::Target, Error> {
        Ok(self.0)
    }
}

impl<T: Default + Copy, const N: usize, const KEY_ID: KeyId, As> Default
    for Indexed<T, N, KEY_ID, As>
{
    fn default() -> Self {
        Self([T::default(); N], PhantomData)
    }
}

/// A decoded NVKV value.
#[derive(Copy, Clone)]
pub(crate) enum DecoderValue<'a> {
    Scalar32(u32),
    Scalar64(u64),
    Array8(&'a [u8]),
    Array32(&'a [u32]),
    Array64(&'a [u64]),
}

macro_rules! impl_try_from_array {
    ($ty:ty, $variant:ident) => {
        impl<'a> TryFrom<DecoderValue<'a>> for $ty {
            type Error = Error;

            fn try_from(value: DecoderValue<'a>) -> Result<Self> {
                if let DecoderValue::$variant(v) = value {
                    Ok(v)
                } else {
                    Err(EINVAL)
                }
            }
        }
    };
}

impl_try_from_array!(u32, Scalar32);
impl_try_from_array!(u64, Scalar64);
impl_try_from_array!(&'a [u8], Array8);
impl_try_from_array!(&'a [u32], Array32);
impl_try_from_array!(&'a [u64], Array64);

/// A visitor that consumes decoded NVKV and produces a `Target`.
pub(crate) trait Schema {
    type Target;

    /// Visits one decoded pair. Returns `Ok(true)` if the schema consumed it.
    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool>;

    /// Returns an initializer that makes the decoded `Target`.
    fn finish(self) -> impl Init<Self::Target, Error>;
}

/// A read position in an NVKV stream.
struct Cursor<'a> {
    data: &'a [u64],
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u64]) -> Self {
        Self { data }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn take_u64(&mut self) -> Result<u64> {
        Ok(self.take_u64s(1)?[0])
    }

    fn take_u8s(&mut self, count: usize) -> Result<&[u8]> {
        let values = self.take_u64s(count.div_ceil(8))?;
        values.as_bytes().get(..count).ok_or(EINVAL)
    }

    fn take_u32s(&mut self, count: usize) -> Result<&[u32]> {
        let values = self.take_u64s(count.div_ceil(2))?;
        // SAFETY: `values` is 8 byte aligned and only 4 byte alignment is required. All bit
        // patterns are valid for `u32`.
        Ok(unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u32>(), count) })
    }

    fn take_u64s(&mut self, count: usize) -> Result<&[u64]> {
        let (prefix, suffix) = self.data.split_at_checked(count).ok_or(EINVAL)?;
        self.data = suffix;
        Ok(prefix)
    }
}

/// A decoder for an NVKV stream.
pub(crate) struct Decoder<'a> {
    data: &'a [u64],
    policy: UnknownKeyPolicy,
}

impl<'a> Decoder<'a> {
    /// Creates a decoder for `data` that handles unknown keys per `policy`.
    pub(crate) fn new(data: &'a [u64], policy: UnknownKeyPolicy) -> Self {
        Self { data, policy }
    }

    fn visit<S: Schema>(
        &self,
        schema: &mut S,
        key: KeyId,
        index: Index,
        value: DecoderValue<'_>,
    ) -> Result {
        let consumed = schema.visit(key, index, value)?;
        if !consumed && self.policy == UnknownKeyPolicy::Error {
            Err(EINVAL)
        } else {
            Ok(())
        }
    }

    fn seq_key(base: KeyId, offset: usize) -> Result<KeyId> {
        base.checked_add(KeyId::try_from(offset)?).ok_or(EINVAL)
    }

    /// Decodes every pair into `schema` and returns the result of [`Schema::finish`].
    pub(crate) fn decode<S: Schema>(&self, mut schema: S) -> Result<impl Init<S::Target, Error>> {
        let mut cursor = Cursor::new(self.data);
        while !cursor.is_empty() {
            let op: Op = cursor.take_u64()?.into();

            let key = op.key().into();
            let index = op.index();
            let op_value: u32 = op.value().into();
            match op.opcode()? {
                Opcode::Imm32 => {
                    self.visit(&mut schema, key, index, DecoderValue::Scalar32(op_value))?;
                }
                Opcode::Seq32 => {
                    let values = cursor.take_u32s(num::u32_as_usize(op_value))?;
                    for (i, &value) in values.iter().enumerate() {
                        let key = Self::seq_key(key, i)?;
                        self.visit(&mut schema, key, index, DecoderValue::Scalar32(value))?;
                    }
                }
                Opcode::Seq64 => {
                    let values = cursor.take_u64s(num::u32_as_usize(op_value))?;
                    for (i, &value) in values.iter().enumerate() {
                        let key = Self::seq_key(key, i)?;
                        self.visit(&mut schema, key, index, DecoderValue::Scalar64(value))?;
                    }
                }
                Opcode::Array8 => {
                    let value = cursor.take_u8s(num::u32_as_usize(op_value))?;
                    self.visit(&mut schema, key, index, DecoderValue::Array8(value))?;
                }
                Opcode::Array32 => {
                    let value = cursor.take_u32s(num::u32_as_usize(op_value))?;
                    self.visit(&mut schema, key, index, DecoderValue::Array32(value))?;
                }
                Opcode::Array64 => {
                    let value = cursor.take_u64s(num::u32_as_usize(op_value))?;
                    self.visit(&mut schema, key, index, DecoderValue::Array64(value))?;
                }
            };
        }
        Ok(schema.finish())
    }
}

/// This is defined per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnknownKeyPolicy {
    Ignore,
    Error,
}
