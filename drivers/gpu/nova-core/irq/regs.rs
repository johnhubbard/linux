// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::io::register;

// GIN, the GPU's interrupt controller: the CPU interrupt tree.
//
// These registers are the two-level CPU interrupt tree at the
// `NV_VIRTUAL_FUNCTION_PRIV` aperture (base `0x00b8_0000`), which any function
// uses to reach its own tree. The leaf arrays have 16 entries, the widest tree
// on any supported part. Pre-Hopper parts implement the first eight, and the
// interrupt HAL supplies the count for a given architecture. See
// `Documentation/gpu/nova/core/interrupts.rst`.

register! {
    /// Latched state of the 32 vectors that belong to one leaf, one bit per vector.
    ///
    /// A read yields the vectors currently latched in leaf `i`. Vector `v` occupies bit `v % 32`
    /// of leaf `v / 32`. Each bit is write-1-to-clear, and a write of `0` does not affect the
    /// value. Each bit must be cleared before its vector is serviced.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF(u32)[16] @ 0x00b81000 {}

    /// Enables individual vectors within one leaf.
    ///
    /// Each `1` written enables the matching vector for delivery to the CPU. Zero bits leave
    /// their vector as it was.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_SET(u32)[16] @ 0x00b81200 {}

    /// Disables individual vectors within one leaf.
    ///
    /// Each `1` written disables the matching vector. The enable governs delivery alone: a
    /// disabled vector still latches in `LEAF`.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_CLEAR(u32)[16] @ 0x00b81400 {}

    /// Enables whole subtrees at the top of the tree.
    ///
    /// Bit `N` covers subtree `N`, which spans leaves `2N` and `2N + 1`. Each `1` written enables
    /// that subtree for delivery to the CPU, and zero bits leave their subtree as it was.
    ///
    /// Hardware defines a single-element array here, and its one element covers subtrees 0 through
    /// 31, every subtree of the widest supported tree. nova-core declares it as a scalar.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET(u32) @ 0x00b81608 {}

    /// Disables whole subtrees at the top of the tree.
    ///
    /// Bit `N` covers subtree `N`. Each `1` written disables that subtree, and zero bits leave
    /// their subtree as it was.
    ///
    /// Hardware defines a single-element array here, and its one element covers subtrees 0 through
    /// 31, every subtree of the widest supported tree. nova-core declares it as a scalar.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR(u32) @ 0x00b81610 {}

    /// Latches a vector from software.
    ///
    /// The vector named in the `vector` field latches in its `LEAF` register exactly as a hardware
    /// source would latch it, and then reaches the CPU under the same enable conditions. The
    /// register is write-only. Every supported part implements it.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_TRIGGER(u32) @ 0x00b81640 {
        /// Vector to latch.
        11:0    vector;
    }
}

// PCI configuration-space mirror, pre-Hopper only.

register! {
    /// MSI end-of-interrupt register.
    ///
    /// A `u32` write rearms MSI delivery on pre-Hopper GPUs. The value is ignored.
    pub(super) NV_XVE_CYA_2(u32) @ 0x0008_8704 {}
}
