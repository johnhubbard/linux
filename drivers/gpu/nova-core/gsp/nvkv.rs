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
}
