// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! 3-level radix page table for data the GSP reads.
//!
//! The GSP bootloader reaches the data by walking three levels of [`GSP_PAGE_SIZE`] pages, each
//! entry an 8-byte little-endian DMA address:
//!
//! ```text
//! Level 0:  one page, one entry  ->  the first level 1 page
//! Level 1:  pages of entries     ->  each entry a level 2 page
//! Level 2:  pages of entries     ->  each entry a data page
//! ```

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

/// A built page table together with the data it maps.
#[pin_data]
pub(crate) struct Radix3 {
    /// The mapped data.
    #[pin]
    data: SGTable<Owned<VVec<u8>>>,
    /// Level 2 entries, one per data page.
    #[pin]
    level2: SGTable<Owned<VVec<u8>>>,
    /// Level 1 entries, one per level 2 page.
    #[pin]
    level1: SGTable<Owned<VVec<u8>>>,
    /// The single level 0 page, whose first entry is the level 1 DMA address.
    level0: Coherent<[u64]>,
    /// Size of [`Self::data`], in bytes.
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

/// Returns the size in bytes of the page table level that maps `sg_table`: one `u64` entry per
/// [`GSP_PAGE_SIZE`] page it spans, rounded up to a whole number of pages.
fn lvl_size(sg_table: &SGTable<Owned<VVec<u8>>>) -> usize {
    let entries: usize = sg_table
        .iter()
        .map(|sg_entry| usize::from_safe_cast(sg_entry.dma_len()).div_ceil(GSP_PAGE_SIZE))
        .sum();

    (entries * size_of::<u64>()).next_multiple_of(GSP_PAGE_SIZE)
}

/// Builds one page table level over `sg_table`: one entry per [`GSP_PAGE_SIZE`] page of every
/// DMA-mapped region, in the order the regions appear.
///
/// The buffer spans a whole number of pages and every byte past the last entry is zero, because
/// the booter reads each level a page at a time.
///
/// # Errors
///
/// - `ENOMEM` if the level cannot be allocated.
/// - `EINVAL` if `sg_table` spans more pages than [`lvl_size`] accounted for.
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
