// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

#![cfg_attr(not(CONFIG_KUNIT), expect(dead_code))]

use core::convert::Infallible;
use core::marker::PhantomData;

use kernel::{
    alloc::ArrayVec,
    prelude::*, //
};
use pin_init::init_array_from_fn;

use crate::gsp::nvkv::{
    Array,
    Index,
    Key,
    KeyId,
    Op,
    Opcode, //
};
use crate::num;

/// Defines a schema struct together with its [`Schema`] implementation that decodes into `$target`.
///
/// Each member of the struct should implement `Schema`. For every (key, index, value) triple
/// decoded from the NVKV stream, the generated parent `Schema` implementation will call each member
/// in declaration order with that triple. If a member consumes that triple, it will stop there.
/// Otherwise it will keep going until all members are tried.
///
/// The schema struct holds the state required by the schema implementation to do the decode. It's
/// recommended to use one of the existing Schema kinds (`Required`, `Accumulated`, `Key`, `Array`,
/// `Indexed`) for each member.
///
/// # Examples
///
/// ```
/// nvkv_decode! {
///     struct RequestSchema => Request {
///         id: Required<u32, 0x0001>,
///         name: Array<u8, 64, 0x0002>,
///     }
/// }
/// ```
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

            fn init() -> impl ::kernel::prelude::Init<Self> {
                ::pin_init::init!(Self {
                    $( $field <- <$ty as $crate::gsp::nvkv::Schema>::init(), )*
                })
            }

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
            fn finish(
                &mut self,
            ) -> impl ::kernel::prelude::Init<Self::Target, ::kernel::error::Error> + '_ {
                let Self { $($field,)* } = self;
                ::kernel::try_init!(Self::Target {
                    $( $field <- $crate::gsp::nvkv::Schema::finish($field), )*
                }? ::kernel::error::Error)
            }
        }

        impl ::core::default::Default for $name {
            fn default() -> Self {
                $crate::gsp::nvkv::assert_schema_size_reasonable::<Self>();
                Self {
                    $( $field: ::core::default::Default::default(), )*
                }
            }
        }
    };
}
pub(crate) use nvkv_decode;

/// Asserts that a schema built by value is small enough.
pub(crate) fn assert_schema_size_reasonable<S>() {
    // Clippy triggers this even if the enclosing function is never called, so skip if clippy is on.
    const_assert!(
        cfg!(clippy) || size_of::<S>() <= 1024,
        "construct large schemas in place with `Schema::init` instead of `Default`"
    );
}

impl<T: for<'a> TryFrom<DecoderValue<'a>, Error = Error> + Default, const KEY_ID: KeyId> Schema
    for Key<T, KEY_ID>
{
    type Target = T;

    #[inline(always)]
    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            Ok(false)
        } else if index != Index::new::<0>() {
            // Single values being set must be at index 0.
            Err(EINVAL)
        } else {
            // Overwrite and take the latest value here.
            self.0 = value.try_into()?;
            Ok(true)
        }
    }

    #[inline(always)]
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
        Ok(core::mem::take(&mut self.0))
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
            // Single values being set must be at index 0.
            Err(EINVAL)
        } else {
            // Overwrite and take the latest value here.
            self.0 = Some(value.try_into()?);
            Ok(true)
        }
    }

    #[inline(always)]
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
        Ok(self.0.take())
    }
}

impl<T: Default + Copy, const N: usize, const KEY_ID: KeyId> Schema for Array<T, N, KEY_ID>
where
    for<'a> &'a [T]: TryFrom<DecoderValue<'a>, Error = Error>,
{
    type Target = ArrayVec<T, N>;

    fn init() -> impl Init<Self> {
        init!(Self {
            vec <- ArrayVec::init_with::<Infallible>(|_| Ok(())),
        })
    }

    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            return Ok(false);
        }
        // Require to be at index 0
        if index != Index::new::<0>() {
            return Err(EINVAL);
        }
        // Reject oversized and take the latest value.
        self.vec.clear();
        self.vec.extend_from_slice(value.try_into()?)?;
        Ok(true)
    }

    #[inline(always)]
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
        ArrayVec::init_with(move |dst| {
            dst.extend_from_slice(&self.vec)?;
            self.vec.clear();
            Ok(())
        })
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
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
        (self.0).0.take().ok_or(EINVAL)
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

    fn take_vec(&mut self) -> Result<KVVec<S::Target>> {
        if self.current_started {
            self.accumulated
                .try_push_init(self.current.finish(), GFP_KERNEL)?;
            self.current_started = false;
        }
        self.current_index = Index::new::<0>();
        Ok(core::mem::take(&mut self.accumulated))
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

            // Require that objects at index k have all their keys sent before the k + 1 th object
            // can be completed. Require that objects are sent contiguously in order from index 0.
            if !self.current_started || index != self.current_index + 1 {
                return Err(EINVAL);
            }

            // The current value must be finished. Push it and swap in `next`.
            self.accumulated
                .try_push_init(self.current.finish(), GFP_KERNEL)?;
            core::mem::swap(&mut self.current, &mut self.next);
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
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
        self.take_vec()
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
    // Reject indices outside of the declared array size.
    let dst = slots.get_mut(start..end).ok_or(EINVAL)?;
    for (d, &e) in dst.iter_mut().zip(elems) {
        *d = T::from(e);
    }
    Ok(())
}

impl<T, const N: usize, const KEY_ID: KeyId, As> Schema for Indexed<T, N, KEY_ID, As>
where
    T: From<As> + Default,
    As: Copy + for<'a> TryFrom<DecoderValue<'a>, Error = Error>,
    for<'a> &'a [As]: TryFrom<DecoderValue<'a>, Error = Error>,
{
    type Target = [T; N];

    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool> {
        if key != KEY_ID {
            return Ok(false);
        }
        let start = index.cast::<usize>().get();
        // Accept both scalar vs scattered array setting for flexibility.
        match <&[As]>::try_from(value) {
            Ok(elems) => scatter_window(&mut self.0, start, elems)?,
            Err(_) => scatter_window(&mut self.0, start, &[As::try_from(value)?])?,
        }
        Ok(true)
    }

    #[inline(always)]
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
        init_array_from_fn(|i| Ok::<_, Error>(core::mem::take(&mut self.0[i])))
    }
}

impl<T: Default + Copy, const N: usize, const KEY_ID: KeyId, As> Default
    for Indexed<T, N, KEY_ID, As>
{
    fn default() -> Self {
        assert_schema_size_reasonable::<Self>();
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

/// Implements `TryFrom` from the given `DecoderValue` variant to the given type.
///
/// `TryFrom` is used by the `Schema` implementations in this file to convert from the
/// `DecoderValue`s into the types to store. Provide the implementations for basic types here.
macro_rules! impl_try_from_decoder_value {
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

impl_try_from_decoder_value!(u32, Scalar32);
impl_try_from_decoder_value!(u64, Scalar64);
impl_try_from_decoder_value!(&'a [u8], Array8);
impl_try_from_decoder_value!(&'a [u32], Array32);
impl_try_from_decoder_value!(&'a [u64], Array64);

/// A visitor that consumes decoded NVKV and produces a `Target`.
pub(crate) trait Schema {
    type Target;

    /// Returns an initializer that creates an empty schema in place.
    ///
    /// Useful if the schema is too large to fit on the stack.
    fn init() -> impl Init<Self>
    where
        Self: Sized + Default,
    {
        Self::default()
    }

    /// Visits one decoded pair. Returns `Ok(true)` if the schema consumed it.
    fn visit<'a>(&mut self, key: KeyId, index: Index, value: DecoderValue<'a>) -> Result<bool>;

    /// Returns an initializer that makes the decoded `Target`.
    ///
    /// After the returned initializer runs successfully, the schema should be empty again. If the
    /// initializer fails, the schema may hold stale state.
    fn finish(&mut self) -> impl Init<Self::Target, Error> + '_;
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
        // PANIC: `take_u64s(1)` returns exactly one element on success.
        Ok(self.take_u64s(1)?[0])
    }

    fn take_u8s(&mut self, count: usize) -> Result<&[u8]> {
        let values = self.take_u64s(count.div_ceil(8))?;
        values.as_bytes().get(..count).ok_or(EINVAL)
    }

    fn take_u32s(&mut self, count: usize) -> Result<&[u32]> {
        let values = self.take_u64s(count.div_ceil(2))?;
        <[u32]>::ref_from_prefix_with_elems(values.as_bytes(), count)
            .map(|(elems, _)| elems)
            .map_err(|_| EINVAL)
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
    pub(crate) fn decode<'s, S: Schema>(
        &self,
        schema: &'s mut S,
    ) -> Result<impl Init<S::Target, Error> + 's> {
        let mut cursor = Cursor::new(self.data);
        while !cursor.is_empty() {
            let op: Op = cursor.take_u64()?.into();

            let key = op.key().into();
            let index = op.index();
            let op_value: u32 = op.value().into();
            match op.opcode()? {
                Opcode::Imm32 => {
                    self.visit(schema, key, index, DecoderValue::Scalar32(op_value))?;
                }
                Opcode::Seq32 => {
                    let values = cursor.take_u32s(num::u32_as_usize(op_value))?;
                    for (i, &value) in values.iter().enumerate() {
                        let key = Self::seq_key(key, i)?;
                        self.visit(schema, key, index, DecoderValue::Scalar32(value))?;
                    }
                }
                Opcode::Seq64 => {
                    let values = cursor.take_u64s(num::u32_as_usize(op_value))?;
                    for (i, &value) in values.iter().enumerate() {
                        let key = Self::seq_key(key, i)?;
                        self.visit(schema, key, index, DecoderValue::Scalar64(value))?;
                    }
                }
                Opcode::Array8 => {
                    let value = cursor.take_u8s(num::u32_as_usize(op_value))?;
                    self.visit(schema, key, index, DecoderValue::Array8(value))?;
                }
                Opcode::Array32 => {
                    let value = cursor.take_u32s(num::u32_as_usize(op_value))?;
                    self.visit(schema, key, index, DecoderValue::Array32(value))?;
                }
                Opcode::Array64 => {
                    let value = cursor.take_u64s(num::u32_as_usize(op_value))?;
                    self.visit(schema, key, index, DecoderValue::Array64(value))?;
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

#[kunit_tests(nova_core_nvkv_decode)]
mod tests {
    use super::*;

    use crate::gsp::nvkv::Encoder;

    // Tests that basic decoding into a manually implemented `Schema` works correctly.
    #[test]
    fn decode_raw_schema() -> Result {
        // Decodes an IMM32 pair and a SEQ64 pair (the encoder emits a u64 as a single-element
        // SEQ64) with a hand written `Schema`. Keys and value constants chosen to distinguish e.g.
        // saving the wrong value to the wrong location.
        const SCALAR32_KEY: KeyId = 0x1001;
        const SCALAR64_KEY: KeyId = 0x1002;
        const UNKNOWN_KEY: KeyId = 0x2001;

        const SCALAR32_VALUE: u32 = 0x1111_2222;
        const SCALAR64_VALUE: u64 = 0x3333_4444_5555_6666;

        // The output type of the hand written Schema. In this case, we can have it also implement
        // `Schema` on itself rather than having a separate carrier type, since the `Schema`
        // implementation is completely stateless.
        #[derive(Default)]
        struct RawSchema {
            scalar32: u32,
            scalar64: u64,
        }

        impl Schema for RawSchema {
            type Target = Self;

            fn visit(&mut self, key: KeyId, index: Index, value: DecoderValue<'_>) -> Result<bool> {
                if index != Index::new::<0>() {
                    return Err(EINVAL);
                }
                match key {
                    SCALAR32_KEY => self.scalar32 = value.try_into()?,
                    SCALAR64_KEY => self.scalar64 = value.try_into()?,
                    _ => return Ok(false),
                }
                Ok(true)
            }

            fn finish(&mut self) -> impl Init<Self::Target, Error> + '_ {
                Ok(core::mem::take(self))
            }
        }

        let mut encoder = Encoder::new();
        encoder.encode_u32(SCALAR32_KEY, Index::new::<0>(), SCALAR32_VALUE)?;
        encoder.encode_u64(SCALAR64_KEY, Index::new::<0>(), SCALAR64_VALUE)?;
        let serialized = encoder.finish();

        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Error);
        let mut schema = RawSchema::default();
        let decoded = KBox::try_init(decoder.decode(&mut schema)?, GFP_KERNEL)?;

        assert_eq!(decoded.scalar32, SCALAR32_VALUE);
        assert_eq!(decoded.scalar64, SCALAR64_VALUE);

        // An unknown key should fail with under `UnknownKeyPolicy::Error` and be skipped under
        // `UnknownKeyPolicy::Ignore`.
        let mut encoder = Encoder::new();
        encoder.encode_u32(UNKNOWN_KEY, Index::new::<0>(), 1)?;

        let serialized = encoder.finish();
        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Error);
        assert!(decoder.decode(&mut RawSchema::default()).is_err());

        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Ignore);
        let mut schema = RawSchema::default();
        let decoded = KBox::try_init(decoder.decode(&mut schema)?, GFP_KERNEL)?;
        assert_eq!(decoded.scalar32, 0);

        Ok(())
    }

    // Tests that decoding via the `nvkv_decode!` macro works correctly.
    #[test]
    fn decode_typed_struct() -> Result {
        const SCALAR32_KEY: KeyId = 0x1234;
        const SCALAR64_KEY: KeyId = 0x1235;
        const ARRAY8_KEY: KeyId = 0x1236;
        const ARRAY32_KEY: KeyId = 0x1237;
        const ARRAY64_KEY: KeyId = 0x1238;
        const OPT_PRESENT_KEY: KeyId = 0x1239;
        const OPT_ABSENT_KEY: KeyId = 0x123a;
        const X_KEY: KeyId = 0x0100;
        const Y_KEY: KeyId = 0x0101;
        const SLOT_KEY: KeyId = 0x0200;

        const SCALAR32_VALUE: u32 = 0x89ab_cdef;
        const SCALAR64_VALUE: u64 = 0x0123_4567_89ab_cdef;
        const ARRAY8_VALUE: &[u8] = &[0x12, 0x34, 0x56];
        const ARRAY32_VALUE: &[u32] = &[0x0123_4567, 0x89ab_cdef];
        const ARRAY64_VALUE: &[u64] = &[0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210];
        const OPT_PRESENT_VALUE: u32 = 0x55;

        nvkv_decode! {
            struct PairSchema => Pair {
                x: Required<u32, { X_KEY }>,
                y: Required<u32, { Y_KEY }>,
            }
        }

        struct Pair {
            x: u32,
            y: u32,
        }

        nvkv_decode! {
            struct TestSchema => TestDecodeable {
                scalar32: Required<u32, { SCALAR32_KEY }>,
                scalar64: Required<u64, { SCALAR64_KEY }>,
                array8: Array<u8, 64, { ARRAY8_KEY }>,
                array32: Array<u32, 64, { ARRAY32_KEY }>,
                array64: Array<u64, 32, { ARRAY64_KEY }>,
                opt_present: Key<Option<u32>, { OPT_PRESENT_KEY }>,
                opt_absent: Key<Option<u32>, { OPT_ABSENT_KEY }>,
                pairs: Accumulated<PairSchema>,
                slots: Indexed<u32, 4, { SLOT_KEY }>,
            }
        }

        struct TestDecodeable {
            scalar32: u32,
            scalar64: u64,
            array8: ArrayVec<u8, 64>,
            array32: ArrayVec<u32, 64>,
            array64: ArrayVec<u64, 32>,
            opt_present: Option<u32>,
            opt_absent: Option<u32>,
            pairs: KVVec<Pair>,
            slots: [u32; 4],
        }

        let index0 = Index::new::<0>();
        let index1 = Index::new::<1>();
        let mut encoder = Encoder::new();
        encoder.encode_u32(SCALAR32_KEY, index0, SCALAR32_VALUE)?;
        encoder.encode_u64(SCALAR64_KEY, index0, SCALAR64_VALUE)?;
        encoder.encode_array8(ARRAY8_KEY, index0, ARRAY8_VALUE)?;
        encoder.encode_array32(ARRAY32_KEY, index0, ARRAY32_VALUE)?;
        encoder.encode_array64(ARRAY64_KEY, index0, ARRAY64_VALUE)?;
        encoder.encode_u32(OPT_PRESENT_KEY, index0, OPT_PRESENT_VALUE)?;
        encoder.encode_u32(X_KEY, index0, 1)?;
        encoder.encode_u32(Y_KEY, index0, 2)?;
        encoder.encode_u32(SLOT_KEY, index1, 20)?;
        encoder.encode_u32(X_KEY, index1, 3)?;
        encoder.encode_u32(Y_KEY, index1, 4)?;
        encoder.encode_u32(SLOT_KEY, index0, 10)?;
        let serialized = encoder.finish();

        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Error);
        let mut schema = TestSchema::default();
        let decoded = KBox::try_init(decoder.decode(&mut schema)?, GFP_KERNEL)?;

        assert_eq!(decoded.scalar32, SCALAR32_VALUE);
        assert_eq!(decoded.scalar64, SCALAR64_VALUE);
        assert_eq!(*decoded.array8, *ARRAY8_VALUE);
        assert_eq!(*decoded.array32, *ARRAY32_VALUE);
        assert_eq!(*decoded.array64, *ARRAY64_VALUE);
        assert_eq!(decoded.opt_present, Some(OPT_PRESENT_VALUE));
        assert_eq!(decoded.opt_absent, None);
        assert_eq!(decoded.pairs.len(), 2);
        assert_eq!(decoded.pairs[0].x, 1);
        assert_eq!(decoded.pairs[0].y, 2);
        assert_eq!(decoded.pairs[1].x, 3);
        assert_eq!(decoded.pairs[1].y, 4);
        assert_eq!(decoded.slots, [10, 20, 0, 0]);

        Ok(())
    }

    // Tests that a schema too large for the stack decodes on the heap.
    #[test]
    fn decode_large_schema_on_heap() -> Result {
        const BLOB_KEY: KeyId = 0x1400;
        const BLOB_VALUE: &[u8] = &[0xab; 100];

        nvkv_decode! {
            struct BigSchema => BigDecodeable {
                blob: Array<u8, 2048, { BLOB_KEY }>,
            }
        }

        struct BigDecodeable {
            blob: ArrayVec<u8, 2048>,
        }

        let mut encoder = Encoder::new();
        encoder.encode_array8(BLOB_KEY, Index::new::<0>(), BLOB_VALUE)?;
        let serialized = encoder.finish();

        let mut schema = KBox::init(BigSchema::init(), GFP_KERNEL)?;
        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Error);
        let decoded = KBox::try_init(decoder.decode(&mut *schema)?, GFP_KERNEL)?;

        assert_eq!(*decoded.blob, *BLOB_VALUE);
        Ok(())
    }
}
