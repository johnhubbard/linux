// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Type-state API for walking the GIN CPU interrupt tree.
//!
//! Each PCIe function has its own interrupt tree, and this module drives one function's CPU tree.
//! A [`Leaf`] carries a type state, `Idle` -> `Pending`, so that clearing one before reading it
//! fails to compile.
//!
//! The type state orders the operations on one [`Leaf`] value. Serializing access to the tree is
//! the caller's responsibility.

use kernel::{
    io::{
        register::Array,
        Io, //
    },
    num::Bounded,
    prelude::*,
};

use crate::{
    driver::Bar0,
    gpu::{
        Architecture,
        Chipset, //
    },
    regs::{
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF as CPU_INTR_LEAF,
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_CLEAR as CPU_INTR_LEAF_EN_CLEAR,
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_SET as CPU_INTR_LEAF_EN_SET,
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_TRIGGER as CPU_INTR_LEAF_TRIGGER,
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR as CPU_INTR_TOP_EN_CLEAR,
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET as CPU_INTR_TOP_EN_SET, //
    },
};

/// Index of a leaf register, bounded to the `0..16` range covered by the leaf register arrays.
pub(super) type LeafIndex = Bounded<usize, 4>;

/// Maps an interrupt `vector` to its position in the tree: the leaf that carries it
/// (`vector / 32`) and the bit index within that leaf (`vector % 32`).
///
/// The returned leaf is a raw index. [`LeafIndex::try_new`] bounds it to the leaf register
/// arrays, and the architecture's leaf count is a separate, narrower bound.
pub(super) const fn vector_leaf_bit(vector: u32) -> (usize, u32) {
    (crate::num::u32_as_usize(vector / 32), vector % 32)
}

/// Maps an interrupt `vector` to the `TOP` enable mask of the subtree that carries it.
///
/// A subtree covers two adjacent leaves, so the vector's leaf is in subtree `vector / 64`. The
/// result has that subtree's bit set, in the form `TOP_EN_SET` and `TOP_EN_CLEAR` take as a
/// value.
///
/// The result is not validated against the subtrees that the architecture supports.
pub(super) const fn vector_subtree_mask(vector: u32) -> u32 {
    1 << (vector / 64)
}

/// Type state of a [`Leaf`] handle: `Idle` before its pending bits are read, `Pending` after.
pub(super) trait State: private::Sealed {}

/// State in which the handle holds no pending bits.
pub(super) struct Idle;
impl State for Idle {}

/// State holding the pending bits read from hardware.
pub(super) struct Pending {
    pending_bits: u32,
}
impl State for Pending {}

mod private {
    pub(in crate::irq) trait Sealed {}
    impl Sealed for super::Idle {}
    impl Sealed for super::Pending {}
}

/// The GIN CPU interrupt tree for a single PCIe function.
#[derive(Clone)]
pub(super) struct Tree {
    /// Number of implemented leaves in this tree, either 8 or 16.
    num_leaves: usize,
    /// Mask of subtree bits the architecture implements.
    subtree_mask: u32,
}

impl Tree {
    /// Creates a `Tree` sized for `chipset`.
    pub(super) fn new(chipset: Chipset) -> Self {
        let num_leaves = match chipset.arch() {
            Architecture::Turing | Architecture::Ampere | Architecture::Ada => 8,
            Architecture::Hopper | Architecture::BlackwellGB10x | Architecture::BlackwellGB20x => {
                16
            }
        };

        Self {
            num_leaves,
            // Each subtree covers two leaves, so one bit per pair of leaves.
            subtree_mask: (1u32 << (num_leaves / 2)) - 1,
        }
    }

    /// Returns a [`Top`] handle for this tree.
    pub(super) fn top(&self) -> Top {
        Top {
            subtree_mask: self.subtree_mask,
        }
    }

    /// Returns a [`Leaf`] handle in the [`Idle`] state for `index`.
    pub(super) fn leaf(&self, index: LeafIndex) -> Leaf<Idle> {
        Leaf::from_index(index)
    }

    /// Injects a software interrupt for `vector` via the trigger register.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `vector` lies outside this tree (`vector >= num_leaves * 32`). `EOVERFLOW` if
    /// `vector` does not fit in the trigger register's vector field.
    pub(super) fn trigger(&self, bar: Bar0<'_>, vector: u32) -> Result {
        if crate::num::u32_as_usize(vector) >= self.num_leaves * 32 {
            return Err(EINVAL);
        }
        bar.write_reg(CPU_INTR_LEAF_TRIGGER::zeroed().try_with_vector(vector)?);
        Ok(())
    }

    /// Clears every pending bit in every implemented leaf.
    ///
    /// The walk runs with every implemented subtree disabled at `TOP`, and every implemented
    /// subtree is enabled on return, whatever its state on entry. The leaves cleared and the
    /// `TOP_EN` writes both reach subtrees the driver does not service.
    ///
    /// Call `drain()` only during probe. It must not run concurrently with an interrupt handler.
    pub(super) fn drain(&self, bar: Bar0<'_>) {
        self.top().disable(bar);

        // `TOP` summarizes enabled leaf bits, so a vector that latched while it was disabled does
        // not appear there.
        for index in 0..(self.num_leaves / 2) {
            for leaf in (Subtree { index }).iter_pending_leaves(self, bar) {
                leaf.clear_pending(bar);
            }
        }

        self.top().enable(bar);
    }
}

/// Top-level view of the interrupt tree, enabling and disabling whole subtrees.
pub(super) struct Top {
    subtree_mask: u32,
}

impl Top {
    /// Enables interrupt delivery for every implemented subtree (`TOP_EN_SET`).
    pub(super) fn enable(self, bar: Bar0<'_>) {
        bar.write(CPU_INTR_TOP_EN_SET, self.subtree_mask.into());
    }

    /// Disables interrupt delivery for every implemented subtree (`TOP_EN_CLEAR`).
    pub(super) fn disable(self, bar: Bar0<'_>) {
        bar.write(CPU_INTR_TOP_EN_CLEAR, self.subtree_mask.into());
    }
}

/// One subtree of the interrupt tree, covering two adjacent leaves.
#[derive(Clone, Copy)]
pub(super) struct Subtree {
    index: usize,
}

impl Subtree {
    /// Yields the two [`Leaf`] handles covered by this subtree.
    fn iter_leaves<'a>(self, tree: &'a Tree) -> impl Iterator<Item = Leaf<Idle>> + 'a {
        // A `Subtree` is constructed only for an implemented index. `LeafIndex::try_new` drops
        // any index beyond the leaf register arrays instead of panicking.
        (0..2usize).filter_map(move |offset| {
            let idx = self.index * 2 + offset;
            LeafIndex::try_new(idx).map(|idx| tree.leaf(idx))
        })
    }

    /// Like [`Self::iter_leaves`], but keeps only leaves with non-zero pending bits.
    pub(super) fn iter_pending_leaves<'a>(
        self,
        tree: &'a Tree,
        bar: Bar0<'a>,
    ) -> impl Iterator<Item = Leaf<Pending>> + 'a {
        self.iter_leaves(tree).filter_map(move |idle| {
            let pending = idle.read_pending(bar);
            (pending.pending_bits() != 0).then_some(pending)
        })
    }
}

/// View of a single interrupt leaf.
pub(super) struct Leaf<S: State = Idle> {
    index: LeafIndex,
    state: S,
}

// The `try_at(...)` calls below cannot fail: `LeafIndex` is `Bounded<usize, 4>`, so its value is
// in 0..16, and every leaf register array has 16 elements.
impl Leaf<Idle> {
    /// Creates a [`Leaf`] handle for `index`.
    pub(super) fn from_index(index: LeafIndex) -> Self {
        Leaf { index, state: Idle }
    }

    /// Enables the vectors set in `vectors` for this leaf (`LEAF_EN_SET`).
    ///
    /// This is the per-vector counterpart of [`Top::enable`], which enables a whole subtree.
    pub(super) fn enable(&self, bar: Bar0<'_>, vectors: u32) {
        if let Some(loc) = CPU_INTR_LEAF_EN_SET::try_at(self.index.get()) {
            bar.write(loc, vectors.into());
        }
    }

    /// Disables the vectors set in `vectors` for this leaf (`LEAF_EN_CLEAR`).
    pub(super) fn disable(&self, bar: Bar0<'_>, vectors: u32) {
        if let Some(loc) = CPU_INTR_LEAF_EN_CLEAR::try_at(self.index.get()) {
            bar.write(loc, vectors.into());
        }
    }

    /// Reads this leaf's pending bits and transitions to [`Pending`].
    pub(super) fn read_pending(self, bar: Bar0<'_>) -> Leaf<Pending> {
        let pending_bits = CPU_INTR_LEAF::try_at(self.index.get())
            .map(|loc| bar.read(loc).into_raw())
            .unwrap_or(0);
        Leaf {
            index: self.index,
            state: Pending { pending_bits },
        }
    }
}

impl Leaf<Pending> {
    /// Returns the pending bits read from hardware.
    pub(super) fn pending_bits(&self) -> u32 {
        self.state.pending_bits
    }

    /// Clears every pending vector by writing its bits back (write-1-to-clear).
    pub(super) fn clear_pending(&self, bar: Bar0<'_>) {
        if self.state.pending_bits != 0 {
            if let Some(loc) = CPU_INTR_LEAF::try_at(self.index.get()) {
                bar.write(loc, self.state.pending_bits.into());
            }
        }
    }
}
