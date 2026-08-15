// SPDX-License-Identifier: GPL-2.0

use kernel::io::register;

use crate::regs::NV_PBUS_SW_SCRATCH;

// PGSP
//
// Msgq v2 holds the four ring pointers below rather than the shared-memory queue headers. Each
// is an array indexed by queue, stride 8 bytes, and nova only uses queue 0, so each is declared
// as a scalar at the queue-0 offset.

register! {
    /// Head of the CPU-to-GSP command ring: where the driver will write next. A write to this
    /// register also rings the GSP's doorbell.
    pub(super) NV_PGSP_QUEUE_HEAD(u32) @ 0x00110c00 {
        31:0    address;
    }
}

register! {
    /// Tail of the CPU-to-GSP command ring: where the GSP will read next.
    pub(super) NV_PGSP_QUEUE_TAIL(u32) @ 0x00110c04 {
        31:0    address;
    }
}

register! {
    /// Head of the GSP-to-CPU message ring: where the GSP will write next.
    pub(super) NV_PGSP_MSGQ_HEAD(u32) @ 0x00110c80 {
        31:0    address;
    }
}

register! {
    /// Tail of the GSP-to-CPU message ring: where the driver will read next.
    pub(super) NV_PGSP_MSGQ_TAIL(u32) @ 0x00110c84 {
        31:0    address;
    }
}

// PBUS

register! {
    /// Scratch register 0xe used as FRTS firmware error code.
    pub(super) NV_PBUS_SW_SCRATCH_0E_FRTS_ERR(u32) => NV_PBUS_SW_SCRATCH[0xe] {
        31:16   frts_err_code;
    }
}
