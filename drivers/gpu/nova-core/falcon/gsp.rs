// SPDX-License-Identifier: GPL-2.0

use kernel::{
    io::{
        poll::read_poll_timeout,
        register::{
            RegisterBase,
            WithBase, //
        },
        Io,
    },
    prelude::*,
    time::Delta, //
};

use crate::{
    driver::Bar0,
    falcon::{
        hal,
        Falcon,
        FalconEngine,
        PFalcon2Base,
        PFalconBase, //
    },
    gpu::Chipset,
    regs,
};

/// Type specifying the `Gsp` falcon engine. Cannot be instantiated.
pub(crate) struct Gsp(());

impl RegisterBase<PFalconBase> for Gsp {
    const BASE: usize = 0x00110000;
}

impl RegisterBase<PFalcon2Base> for Gsp {
    const BASE: usize = 0x00111000;
}

impl FalconEngine for Gsp {}

impl Gsp {
    /// Clears the GSP falcon SWGEN0 interrupt latch.
    ///
    /// The latch holds until it is cleared, and the GSP drives no new edge into the interrupt
    /// tree while it is set, so a caller that consumed a notification by any means other than the
    /// interrupt handler must clear it or no further notification is delivered.
    pub(crate) fn clear_swgen0_intr(bar: Bar0<'_>) {
        bar.write(
            WithBase::of::<Self>(),
            regs::NV_PFALCON_FALCON_IRQSCLR::zeroed().with_swgen0(true),
        );
    }

    /// Reads the GSP falcon interrupt causes pending for the host, clearing the SWGEN0 latch if
    /// it was set.
    ///
    /// Returns the causes as they were read, before the clear. The GSP raises SWGEN0 when it has
    /// posted messages in the GSP-to-CPU queue, so any other cause here is something else, for
    /// example a HALT from a GSP crash. Causes the falcon routes to its own RISC-V core belong to
    /// the firmware and are excluded.
    pub(crate) fn take_host_intr(
        bar: Bar0<'_>,
        chipset: Chipset,
    ) -> regs::NV_PFALCON_FALCON_IRQSTAT {
        let latched = bar.read(regs::NV_PFALCON_FALCON_IRQSTAT::of::<Self>());
        let status = regs::NV_PFALCON_FALCON_IRQSTAT::from(
            latched.into_raw() & hal::host_intr_routing::<Self>(bar, chipset),
        );

        if status.swgen0() {
            Self::clear_swgen0_intr(bar);
        }

        status
    }

    /// Clears the latch of every interrupt cause set in `status`.
    ///
    /// A cause left latched holds the falcon's host-routed set non-empty, and the falcon signals
    /// the tree only on a transition of that set.
    pub(crate) fn clear_intr(bar: Bar0<'_>, status: regs::NV_PFALCON_FALCON_IRQSTAT) {
        bar.write(
            WithBase::of::<Self>(),
            regs::NV_PFALCON_FALCON_IRQSCLR::from(status.into_raw()),
        );
    }

    /// Re-emits the falcon's host-routed interrupt causes into the interrupt tree.
    ///
    /// The falcon signals the tree on a transition of those causes, so clearing the tree leaf
    /// while a cause is still latched leaves no transition and no further vector.
    ///
    /// Does nothing on Turing, whose falcons do not implement the register.
    pub(crate) fn retrigger_intr(bar: Bar0<'_>, chipset: Chipset) {
        if !hal::has_intr_retrigger(chipset) {
            return;
        }

        bar.write(
            WithBase::of::<Self>().at(0),
            regs::NV_PFALCON_FALCON_INTR_RETRIGGER::zeroed().with_trigger(true),
        );
    }
}

impl<'a> Falcon<'a, Gsp> {
    /// Checks if GSP reload/resume has completed during the boot process.
    pub(crate) fn check_reload_completed(&self, timeout: Delta) -> Result<bool> {
        read_poll_timeout(
            || Ok(self.bar.read(regs::NV_PGC6_BSI_SECURE_SCRATCH_14)),
            |val| val.boot_stage_3_handoff(),
            Delta::ZERO,
            timeout,
        )
        .map(|_| true)
    }

    /// Returns whether the RISC-V branch privilege lockdown bit is set.
    pub(crate) fn riscv_branch_privilege_lockdown(&self) -> bool {
        self.bar
            .read(regs::NV_PFALCON_FALCON_HWCFG2::of::<Gsp>())
            .riscv_br_priv_lockdown()
    }

    /// Returns whether GSP registers can be read by the CPU.
    pub(crate) fn priv_target_mask_released(&self) -> bool {
        /// Pattern returned by GSP register reads while the PRIV target mask still blocks CPU
        /// access. The low byte varies; the upper 24 bits are fixed.
        const LOCKED_PATTERN: u32 = 0xbadf_4100;
        const LOCKED_MASK: u32 = 0xffff_ff00;

        let hwcfg2 = self
            .bar
            .read(regs::NV_PFALCON_FALCON_HWCFG2::of::<Gsp>())
            .into_raw();

        hwcfg2 != 0 && (hwcfg2 & LOCKED_MASK) != LOCKED_PATTERN
    }
}
