// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! 3-level radix page table for GSP firmware data.
//!
//! The GSP bootloader expects data to be mapped via a 3-level page table:
//!
//! ```text
//! Level 0:  1 page, 1 entry         -> points to first level 1 page
//! Level 1:  Multiple pages/entries  -> each entry points to a level 2 page
//! Level 2:  Multiple pages/entries  -> each entry points to a data page
//! ```
//!
//! Each page is 4KB, each entry is 8 bytes (64-bit DMA address).

use core::mem::size_of;

use kernel::{
    device,
    dma::{
        Coherent,
        CoherentBox,
        DataDirection,
        DmaAddress, //
    },
    prelude::*,
    scatterlist::{
        Owned,
        SGTable, //
    },
};

use crate::{
    gsp::GSP_PAGE_SIZE,
    num::FromSafeCast, //
};

/// 3-level radix page table mapping arbitrary data for the GSP.
#[pin_data]
pub(crate) struct Radix3 {
    /// The data mapped via a SG table.
    #[pin]
    data: SGTable<Owned<VVec<u8>>>,
    /// Level 2 page table whose entries contain DMA addresses of data pages.
    #[pin]
    level2: SGTable<Owned<VVec<u8>>>,
    /// Level 1 page table whose entries contain DMA addresses of level 2 pages.
    #[pin]
    level1: SGTable<Owned<VVec<u8>>>,
    /// Level 0 page table (single 4KB page) with one entry: DMA address of first level 1 page.
    level0: Coherent<[u64]>,
    /// Size in bytes of the data contained in [`Self::data`].
    size: usize,
}

impl Radix3 {
    /// Builds a 3-level radix page table that maps `data` into `dev`'s DMA address space.
    ///
    /// Takes ownership of `data`. May sleep.
    pub(crate) fn new<'a>(
        dev: &'a device::Device<device::Bound>,
        data: VVec<u8>,
    ) -> impl PinInit<Self, Error> + 'a {
        let size = data.len();

        pin_init::pin_init_scope(move || {
            Ok(try_pin_init!(Self {
                data <- SGTable::new(dev, data, DataDirection::ToDevice, GFP_KERNEL),
                level2 <- {
                    let level2 = build_lvl(&data)?;

                    SGTable::new(dev, level2, DataDirection::ToDevice, GFP_KERNEL)
                },
                level1 <- {
                    let level1 = build_lvl(&level2)?;

                    SGTable::new(dev, level1, DataDirection::ToDevice, GFP_KERNEL)
                },
                level0: {
                    let level1_entry = level1.iter().next().ok_or(EINVAL)?;
                    let level1_entry_addr = level1_entry.dma_address();

                    let mut level0 = CoherentBox::<[u64]>::zeroed_slice(
                        dev,
                        GSP_PAGE_SIZE / size_of::<u64>(),
                        GFP_KERNEL,
                    )?;
                    level0[0] = level1_entry_addr.to_le();

                    level0.into()
                },
                size,
            }))
        })
    }

    /// Returns the DMA address of the radix3 level 0 page table.
    pub(crate) fn dma_address(&self) -> DmaAddress {
        self.level0.dma_address()
    }

    /// Returns the size of the mapped data, in bytes.
    pub(crate) fn size(&self) -> usize {
        self.size
    }
}

/// Returns the size, in bytes, of the page table level that maps `sg_table`: one `u64` entry per
/// 4KB page it spans, rounded up to a whole number of `GSP_PAGE_SIZE` pages.
fn lvl_size(sg_table: &SGTable<Owned<VVec<u8>>>) -> usize {
    let entries: usize = sg_table
        .iter()
        .map(|sg_entry| usize::from_safe_cast(sg_entry.dma_len()).div_ceil(GSP_PAGE_SIZE))
        .sum();

    (entries * size_of::<u64>()).next_multiple_of(GSP_PAGE_SIZE)
}

/// Builds a page table level from a scatter-gather list.
///
/// Takes each DMA-mapped region from `sg_table` and writes page table entries
/// for all 4KB pages within that region. For example, a 16KB SG entry becomes
/// 4 consecutive page table entries.
///
/// The returned buffer spans a whole number of `GSP_PAGE_SIZE` pages, and every byte past the
/// last entry is zero. The booter DMAs each level a whole page at a time.
///
/// Returns `ENOMEM` if the level cannot be allocated, and `EINVAL` if `sg_table` spans more
/// pages than [`lvl_size`] accounted for.
fn build_lvl(sg_table: &SGTable<Owned<VVec<u8>>>) -> Result<VVec<u8>> {
    let mut dst = VVec::<u8>::zeroed(lvl_size(sg_table), GFP_KERNEL).map_err(|_| ENOMEM)?;
    let mut entries = dst.chunks_exact_mut(size_of::<u64>());

    for sg_entry in sg_table.iter() {
        let num_pages = usize::from_safe_cast(sg_entry.dma_len()).div_ceil(GSP_PAGE_SIZE);

        for i in 0..num_pages {
            let entry = sg_entry.dma_address()
                + (u64::from_safe_cast(i) * u64::from_safe_cast(GSP_PAGE_SIZE));

            entries
                .next()
                .ok_or(EINVAL)?
                .copy_from_slice(&entry.to_le_bytes());
        }
    }

    Ok(dst)
}
