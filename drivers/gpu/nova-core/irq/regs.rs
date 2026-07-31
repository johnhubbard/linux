// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::io::register;

use super::interrupt_tree::{
    LeafMask,
    SubtreeSet, //
};

// The GIN CPU interrupt tree, at the `NV_VIRTUAL_FUNCTION_PRIV` aperture that any function uses to
// reach its own tree. The leaf arrays are declared with 16 entries, the widest tree on any
// supported part, and the interrupt HAL supplies the count an architecture implements.

register! {
    /// Latched state of the 32 vectors that belong to one leaf, one bit per vector.
    ///
    /// Vector `v` occupies bit `v % 32` of leaf `v / 32`. Each bit is write-1-to-clear, and a
    /// write of `0` leaves its bit as it was.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF(u32)[16] @ 0x00b81000 {
        /// Vectors latched in this leaf.
        31:0    vectors => LeafMask;
    }

    /// Enables individual vectors within one leaf.
    ///
    /// Each `1` written enables the matching vector for delivery to the CPU. Zero bits leave
    /// their vector as it was.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_SET(u32)[16] @ 0x00b81200 {
        /// Vectors to enable.
        31:0    vectors => LeafMask;
    }

    /// Disables individual vectors within one leaf.
    ///
    /// Each `1` written disables the matching vector. The enable governs delivery alone: a
    /// disabled vector still latches in `LEAF`.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_CLEAR(u32)[16] @ 0x00b81400 {
        /// Vectors to disable.
        31:0    vectors => LeafMask;
    }

    /// Enables whole subtrees at the top of the tree.
    ///
    /// Bit `N` covers subtree `N`, which spans leaves `2N` and `2N + 1`. Each `1` written enables
    /// that subtree for delivery to the CPU, and zero bits leave their subtree as it was.
    ///
    /// Hardware declares a single-element array whose one element covers every subtree, so
    /// nova-core declares a scalar.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET(u32) @ 0x00b81608 {
        /// Subtrees to enable.
        31:0    subtrees => SubtreeSet;
    }

    /// Disables whole subtrees at the top of the tree.
    ///
    /// Bit `N` covers subtree `N`. Each `1` written disables that subtree, and zero bits leave
    /// their subtree as it was.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR(u32) @ 0x00b81610 {
        /// Subtrees to disable.
        31:0    subtrees => SubtreeSet;
    }

    /// Latches a vector from software.
    ///
    /// The vector named in the `vector` field latches in its `LEAF` register exactly as a hardware
    /// source would, and reaches the CPU under the same enables. Write-only, and implemented on
    /// every supported part.
    pub(super) NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_TRIGGER(u32) @ 0x00b81640 {
        /// Vector to latch.
        11:0    vector;
    }
}

// PCI configuration-space mirror.

register! {
    /// MSI end-of-interrupt register.
    ///
    /// A `u32` write rearms MSI delivery on pre-Hopper GPUs. The value is ignored.
    pub(super) NV_XVE_CYA_2(u32) @ 0x00088704 {}
}
