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
}
