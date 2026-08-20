// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Blackwell GB10x framebuffer HAL.

use kernel::{
    io::{
        register::{
            RegisterBase,
            WithBase, //
        },
        Io, //
    },
    num::Bounded,
    prelude::*,
    ptr::{
        const_align_up,
        Alignment, //
    },
    sizes::*, //
};

use crate::{
    driver::Bar0,
    fb::{
        hal::FbHal,
        regs, //
    },
    num::usize_into_u32,
};

struct Gb100;

impl RegisterBase<regs::Hshub0Base> for Gb100 {
    const BASE: usize = 0x0087_0000;
}

fn read_sysmem_flush_page_gb100(bar: Bar0<'_>) -> u64 {
    let lo = u64::from(
        bar.read(regs::NV_PFB_HSHUB_PCIE_FLUSH_SYSMEM_ADDR_LO::of::<Gb100>())
            .adr(),
    );
    let hi = u64::from(
        bar.read(regs::NV_PFB_HSHUB_PCIE_FLUSH_SYSMEM_ADDR_HI::of::<Gb100>())
            .adr(),
    );

    lo | (hi << 32)
}

/// Write the sysmem flush page address through the GB10x HSHUB0 registers.
///
/// Both the primary and EG (egress) register pairs must be programmed to the same address,
/// as required by hardware.
fn write_sysmem_flush_page_gb100(bar: Bar0<'_>, addr: Bounded<u64, 52>) {
    // CAST: lower 32 bits. Hardware ignores bits 7:0.
    let addr_lo = *addr as u32;
    let addr_hi = addr.shr::<32, 20>().cast::<u32>();

    // Write HI first. The hardware will trigger the flush on the LO write.

    // Primary HSHUB pair.
    bar.write(
        regs::NV_PFB_HSHUB_PCIE_FLUSH_SYSMEM_ADDR_HI::of::<Gb100>(),
        regs::NV_PFB_HSHUB_PCIE_FLUSH_SYSMEM_ADDR_HI::zeroed().with_adr(addr_hi),
    );
    bar.write(
        regs::NV_PFB_HSHUB_PCIE_FLUSH_SYSMEM_ADDR_LO::of::<Gb100>(),
        regs::NV_PFB_HSHUB_PCIE_FLUSH_SYSMEM_ADDR_LO::zeroed().with_adr(addr_lo),
    );

    // EG (egress) pair -- must match the primary pair.
    bar.write(
        regs::NV_PFB_HSHUB_EG_PCIE_FLUSH_SYSMEM_ADDR_HI::of::<Gb100>(),
        regs::NV_PFB_HSHUB_EG_PCIE_FLUSH_SYSMEM_ADDR_HI::zeroed().with_adr(addr_hi),
    );
    bar.write(
        regs::NV_PFB_HSHUB_EG_PCIE_FLUSH_SYSMEM_ADDR_LO::of::<Gb100>(),
        regs::NV_PFB_HSHUB_EG_PCIE_FLUSH_SYSMEM_ADDR_LO::zeroed().with_adr(addr_lo),
    );
}

/// PMU backing store, per Open RM's `kpmuReservedMemoryBackingStoreSizeGet`.
const PMU_BACKING_STORE_SIZE: usize = 9 * SZ_1M;

/// PMU communication surfaces, per Open RM's `gpuGetPmuReservedMemorySurfacesSize`.
const PMU_SURFACES_SIZE: usize = SZ_16M + SZ_256K;

/// Miscellaneous PMU memory, per Open RM's `kpmuReservedMemoryMiscSizeGet`.
const PMU_MISC_SIZE: usize = SZ_4K;

/// Open RM reserves the backing store, the surfaces and the misc memory as one region above
/// FRTS, aligned to `KPMU_RESERVED_MEMORY_ALIGNMENT` in `kpmuReservedMemorySizeGet`.
pub(super) const fn pmu_reserved_size_gb100() -> u32 {
    usize_into_u32::<
        {
            const_align_up(
                PMU_BACKING_STORE_SIZE + PMU_SURFACES_SIZE + PMU_MISC_SIZE,
                Alignment::new::<SZ_128K>(),
            )
            .unwrap()
        },
    >()
}

impl FbHal for Gb100 {
    fn read_sysmem_flush_page(&self, bar: Bar0<'_>) -> u64 {
        read_sysmem_flush_page_gb100(bar)
    }

    fn write_sysmem_flush_page(&self, bar: Bar0<'_>, addr: u64) -> Result {
        let addr = Bounded::<u64, 52>::try_new(addr).ok_or(EINVAL)?;

        write_sysmem_flush_page_gb100(bar, addr);

        Ok(())
    }

    fn supports_display(&self, bar: Bar0<'_>) -> bool {
        super::ga100::display_enabled_ga100(bar)
    }

    fn vidmem_size(&self, bar: Bar0<'_>) -> u64 {
        super::ga102::vidmem_size_ga102(bar)
    }

    fn pmu_reserved_size(&self) -> u32 {
        pmu_reserved_size_gb100()
    }

    fn non_wpr_heap_size(&self) -> u64 {
        // Open RM's `kgspGetNonWprHeapSize` returns a flat 2 MiB for GB10x.
        u64::SZ_2M
    }

    fn frts_size(&self) -> u64 {
        super::tu102::frts_size_tu102()
    }
}

const GB100: Gb100 = Gb100;
pub(super) const GB100_HAL: &dyn FbHal = &GB100;
