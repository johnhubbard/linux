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
                    VVec::<u8>::with_capacity(
                        data.iter().count() * core::mem::size_of::<u64>(),
                        GFP_KERNEL,
                    )
                    .map_err(|_| ENOMEM)
                    .and_then(|level2| map_into_lvl(&data, level2))
                    .map(|level2| SGTable::new(dev, level2, DataDirection::ToDevice, GFP_KERNEL))?
                },
                level1 <- {
                    VVec::<u8>::with_capacity(
                        level2.iter().count() * core::mem::size_of::<u64>(),
                        GFP_KERNEL,
                    )
                    .map_err(|_| ENOMEM)
                    .and_then(|level1| map_into_lvl(&level2, level1))
                    .map(|level1| SGTable::new(dev, level1, DataDirection::ToDevice, GFP_KERNEL))?
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

/// Build a page table from a scatter-gather list.
///
/// Takes each DMA-mapped region from `sg_table` and writes page table entries
/// for all 4KB pages within that region. For example, a 16KB SG entry becomes
/// 4 consecutive page table entries.
fn map_into_lvl(sg_table: &SGTable<Owned<VVec<u8>>>, mut dst: VVec<u8>) -> Result<VVec<u8>> {
    for sg_entry in sg_table.iter() {
        let num_pages = usize::from_safe_cast(sg_entry.dma_len()).div_ceil(GSP_PAGE_SIZE);

        for i in 0..num_pages {
            let entry = sg_entry.dma_address()
                + (u64::from_safe_cast(i) * u64::from_safe_cast(GSP_PAGE_SIZE));
            dst.extend_from_slice(&entry.to_le_bytes(), GFP_KERNEL)?;
        }
    }

    Ok(dst)
}
