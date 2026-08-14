// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Codec for NVKV, the binary key-value format of GMCAPI.
//!
//! Essentially, the format encodes a sequence of calls to some function f(key, index, value),
//! where value is a [u8], u32, u64, [u32], or a [u64]. The key is a u16 and the index is a 12 bit
//! integer. The interpretation of these function calls is per GMCAPI. Generally speaking, the
//! function calls will map to some struct - for example, f(GPU_NAME_STRING_KEY, 0, b"some gpu")
//! naturally maps to storing a &str with the GPU name.

use kernel::prelude::*;

mod encode;
pub(crate) use encode::*;

mod types;
pub(crate) use types::*;

mod decode;
pub(crate) use decode::*;

#[kunit_tests(nova_core_nvkv)]
mod tests {
    use super::*;

    #[test]
    fn encode_all_value_kinds() -> Result {
        let mut encoder = Encoder::new();

        encoder.encode_u32(0x1001, Index::new::<0>(), 0x1111_2222)?;
        encoder.encode_u64(0x1002, Index::new::<1>(), 0x3333_4444_5555_6666)?;
        encoder.encode_array8(0x1003, Index::new::<2>(), &[0xaa, 0xbb, 0xcc])?;
        encoder.encode_array32(0x1005, Index::new::<4>(), &[0xbbbb_cccc, 0xdddd_eeee])?;
        encoder.encode_array64(
            0x1006,
            Index::new::<5>(),
            &[0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210],
        )?;

        let encoded = encoder.finish();
        let expected: &[u64] = &[
            0x1111_2222_0000_1001,
            0x0000_0001_2001_1002,
            0x3333_4444_5555_6666,
            0x0000_0003_3002_1003,
            0x0000_0000_00cc_bbaa,
            0x0000_0002_4004_1005,
            0xdddd_eeee_bbbb_cccc,
            0x0000_0002_5005_1006,
            0x0123_4567_89ab_cdef,
            0xfedc_ba98_7654_3210,
        ];
        assert_eq!(&*encoded, expected);

        Ok(())
    }

    #[test]
    fn decode_raw_schema() -> Result {
        const SCALAR32_KEY: KeyId = 0x1234;
        const SEQ32_KEY0: KeyId = 0x1237;
        const SEQ32_KEY1: KeyId = 0x1238;

        const SCALAR32_VALUE: u32 = 0x89ab_cdef;

        #[derive(Default)]
        struct RawSchema {
            scalar32: u32,
            seq32_0: u32,
            seq32_1: u32,
        }

        impl Schema for RawSchema {
            type Target = Self;

            fn visit(&mut self, key: KeyId, index: Index, value: DecoderValue<'_>) -> Result<bool> {
                // Stability: Single values being set must be at index 0.
                if index != Index::new::<0>() {
                    return Err(EINVAL);
                }
                match key {
                    SCALAR32_KEY => self.scalar32 = value.try_into()?,
                    SEQ32_KEY0 => self.seq32_0 = value.try_into()?,
                    SEQ32_KEY1 => self.seq32_1 = value.try_into()?,
                    _ => return Ok(false),
                }
                Ok(true)
            }

            fn finish(self) -> impl Init<Self::Target, Error> {
                Ok(self)
            }
        }

        let index = Index::new::<0>();
        let mut encoder = Encoder::new();
        encoder.encode_u32(SCALAR32_KEY, index, SCALAR32_VALUE)?;
        let mut serialized = encoder.finish();
        // A hand-built SEQ32 pair: count 2 at `SEQ32_KEY0`, one data word with both values.
        serialized.push(0x0000_0002_1000_1237, GFP_KERNEL)?;
        serialized.push(0x3333_4444_1111_2222, GFP_KERNEL)?;

        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Error);
        let decoded = KBox::try_init(decoder.decode(RawSchema::default())?, GFP_KERNEL)?;

        assert_eq!(decoded.scalar32, SCALAR32_VALUE);
        assert_eq!(decoded.seq32_0, 0x1111_2222);
        assert_eq!(decoded.seq32_1, 0x3333_4444);

        let mut encoder = Encoder::new();
        encoder.encode_u32(0xffff, index, 1)?;
        let serialized = encoder.finish();
        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Error);
        assert!(decoder.decode(RawSchema::default()).is_err());
        let decoder = Decoder::new(&serialized, UnknownKeyPolicy::Ignore);
        let decoded = KBox::try_init(decoder.decode(RawSchema::default())?, GFP_KERNEL)?;
        assert_eq!(decoded.scalar32, 0);

        Ok(())
    }

    #[test]
    fn encode_typed_struct() -> Result {
        const U32_KEY: KeyId = 0x0001;
        const U64_KEY: KeyId = 0x0002;
        const NAME_KEY: KeyId = 0x0003;
        const FIXED_KEY: KeyId = 0x0004;
        const OPT_KEY: KeyId = 0x0005;

        nvkv_encode! {
            struct TypedRequest {
                a: Key<u32, { U32_KEY }>,
                b: Key<u64, { U64_KEY }>,
                name: Key<&'static [u8], { NAME_KEY }>,
                fixed: Key<[u8; 4], { FIXED_KEY }>,
                opt: Option<Key<u32, { OPT_KEY }>>,
            }
        }

        let request = TypedRequest {
            a: 0x89ab_cdef.into(),
            b: 0x0123_4567_89ab_cdef.into(),
            name: b"name\0".into(),
            fixed: [1u8, 2, 3, 4].into(),
            opt: None,
        };

        let mut encoder = Encoder::new();
        request.encode(&mut encoder)?;
        let encoded = encoder.finish();

        assert_eq!(encoded.len(), 7);

        Ok(())
    }

    #[test]
    fn decode_test() -> Result {
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
            #[derive(Default)]
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
            #[derive(Default)]
            struct TestSchema => TestDecodeable {
                scalar32: Required<u32, { SCALAR32_KEY }>,
                scalar64: Required<u64, { SCALAR64_KEY }>,
                array8: Array<u8, 64, { ARRAY8_KEY }>,
                array32: Array<u32, 64, { ARRAY32_KEY }>,
                array64: Array<u64, 64, { ARRAY64_KEY }>,
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
            array64: ArrayVec<u64, 64>,
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
        let decoded = KBox::try_init(decoder.decode(TestSchema::default())?, GFP_KERNEL)?;

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
}
