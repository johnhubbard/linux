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
    pci::IrqType,
    prelude::*,
};

use crate::{
    driver::Bar0,
    gpu::Chipset,
    irq::hal::{
        cpu_interrupt_hal,
        PciIrqRearmMethod, //
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
    /// The subtrees this tree enables and services.
    serviced_subtrees: u32,
    /// Method that rearms PCI interrupt delivery, or `None` if the interrupt type needs no rearm
    /// write.
    rearm_method: Option<PciIrqRearmMethod>,
}

impl Tree {
    /// Creates a `Tree` for `chipset` covering `serviced_subtrees`, with the rearm method that
    /// `irq_type` requires.
    ///
    /// Each serviced subtree must have an allocated PCI vector and a registered handler, which
    /// [`super::alloc_vectors`] sizes the allocation for. Bits outside the subtrees the
    /// architecture implements are dropped.
    pub(super) fn new(chipset: Chipset, irq_type: IrqType, serviced_subtrees: u32) -> Self {
        let hal = cpu_interrupt_hal(chipset);
        Self {
            num_leaves: hal.num_leaves(),
            serviced_subtrees: serviced_subtrees & hal.implemented_subtrees(),
            rearm_method: hal.pci_irq_rearm_method(irq_type),
        }
    }

    /// Rearms PCI interrupt delivery to the CPU after servicing `subtree`, the `TOP` bit of the
    /// one subtree the calling handler serves.
    ///
    /// A handler must call this before it returns, or it receives no further interrupts.
    pub(super) fn rearm_pci_irq(&self, bar: Bar0<'_>, subtree: u32) {
        if let Some(method) = self.rearm_method {
            method.rearm(bar, self.serviced_subtrees, subtree);
        }
    }

    /// Returns a [`Top`] handle for this tree.
    pub(super) fn top(&self) -> Top {
        Top {
            serviced_subtrees: self.serviced_subtrees,
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
    // Only the interrupt self-test injects a software interrupt.
    #[cfg_attr(not(CONFIG_NOVA_CORE_IRQ_SELFTEST), expect(dead_code))]
    pub(super) fn trigger(&self, bar: Bar0<'_>, vector: u32) -> Result {
        if crate::num::u32_as_usize(vector) >= self.num_leaves * 32 {
            return Err(EINVAL);
        }
        bar.write_reg(CPU_INTR_LEAF_TRIGGER::zeroed().try_with_vector(vector)?);
        Ok(())
    }

    /// Disables every vector in every implemented leaf (`LEAF_EN_CLEAR`).
    ///
    /// Boot, or a driver that ran before this one, can leave leaf enables set for vectors
    /// nova-core does not service, and such a vector delivers to nova-core's handler once its
    /// subtree is enabled.
    ///
    /// This clears enables outside the subtrees nova-core services, so it is a probe-time
    /// operation only.
    pub(super) fn disable_all_leaves(&self, bar: Bar0<'_>) {
        for index in 0..self.num_leaves {
            if let Some(index) = LeafIndex::try_new(index) {
                self.leaf(index).disable(bar, u32::MAX);
            }
        }
    }

    /// Clears every pending bit in every implemented leaf.
    ///
    /// Disables this tree's serviced subtrees at `TOP` across the walk, then enables them,
    /// whatever their state on entry. The leaves cleared reach subtrees the driver does not
    /// service, and the `TOP_EN` writes do not.
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
///
/// Both writes cover the serviced subtrees alone, leaving the rest of the tree as it was.
pub(super) struct Top {
    serviced_subtrees: u32,
}

impl Top {
    /// Enables this tree's serviced subtrees (`TOP_EN_SET`).
    pub(super) fn enable(self, bar: Bar0<'_>) {
        bar.write(CPU_INTR_TOP_EN_SET, self.serviced_subtrees.into());
    }

    /// Disables this tree's serviced subtrees (`TOP_EN_CLEAR`).
    pub(super) fn disable(self, bar: Bar0<'_>) {
        bar.write(CPU_INTR_TOP_EN_CLEAR, self.serviced_subtrees.into());
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

    /// Clears the vectors set in `vectors` (write-1-to-clear), leaving every other pending bit
    /// set.
    ///
    /// A handler that services one vector uses this rather than [`Self::clear_pending`], which
    /// clears every vector the leaf had pending.
    pub(super) fn clear_vectors(&self, bar: Bar0<'_>, vectors: u32) {
        if vectors != 0 {
            if let Some(loc) = CPU_INTR_LEAF::try_at(self.index.get()) {
                bar.write(loc, vectors.into());
            }
        }
    }
}

#[kunit_tests(nova_core_gin_tree)]
mod tests {
    use super::*;

    /// A leaf index is a `Bounded<usize, 4>`, so it accepts 0..=15 and rejects 16.
    #[test]
    fn leaf_index_bounds() {
        assert!(LeafIndex::try_new(0).is_some());
        assert!(LeafIndex::try_new(15).is_some());
        assert!(LeafIndex::try_new(16).is_none());
    }

    /// Subtree `N` covers the two adjacent leaves `2N` and `2N + 1`.
    #[test]
    fn subtree_covers_two_adjacent_leaves() {
        let tree = Tree {
            num_leaves: 16,
            serviced_subtrees: 0xff,
            rearm_method: None,
        };

        for index in 0..8usize {
            let mut leaves = Subtree { index }.iter_leaves(&tree);
            assert_eq!(leaves.next().map(|leaf| leaf.index.get()), Some(index * 2));
            assert_eq!(
                leaves.next().map(|leaf| leaf.index.get()),
                Some(index * 2 + 1)
            );
            assert!(leaves.next().is_none());
        }
    }

    /// Leaves that fall outside the addressable range are filtered out, never panicking. The
    /// filter is the [`LeafIndex`] bound, not the tree's leaf count, so this holds even on the
    /// widest tree.
    #[test]
    fn subtree_leaves_out_of_range_are_filtered() {
        let tree = Tree {
            num_leaves: 16,
            serviced_subtrees: 0xff,
            rearm_method: None,
        };

        // Subtree 8 would cover leaves 16 and 17, both beyond the leaf index range.
        assert!(Subtree { index: 8 }.iter_leaves(&tree).next().is_none());
    }

    /// The production [`vector_leaf_bit`] maps every vector to a `(leaf, bit)` pair, valid leaves
    /// stay within [`LeafIndex`], and the fixed doorbell (129) and GSP (155) vectors land where
    /// the handlers expect.
    #[test]
    fn vector_maps_to_leaf_and_bit() {
        // Every vector of a 16-leaf tree maps to an addressable leaf and a bit in 0..32.
        for vector in 0u32..(16 * 32) {
            let (leaf, bit) = vector_leaf_bit(vector);

            assert!(LeafIndex::try_new(leaf).is_some());
            assert!(bit < 32);
            assert_eq!(leaf as u32 * 32 + bit, vector);
        }

        // The fixed vectors the handlers rely on: CPU doorbell 129 and GSP notification 155, both
        // in leaf 4, which is present on both the 8-leaf (pre-Hopper) and 16-leaf trees.
        assert_eq!(vector_leaf_bit(129), (4, 1));
        assert_eq!(vector_leaf_bit(155), (4, 27));
        assert!(LeafIndex::try_new(vector_leaf_bit(155).0).is_some());

        // The first vector beyond the 16-leaf tree lands in leaf 16, which is out of range.
        assert!(LeafIndex::try_new(vector_leaf_bit(16 * 32).0).is_none());
    }

    /// [`vector_subtree_mask`] agrees with [`vector_leaf_bit`] on which subtree holds a vector,
    /// and the doorbell (129) and GSP (155) vectors share one, so a single allocation and a single
    /// enabled subtree serve both.
    #[test]
    fn vector_maps_to_subtree() {
        for vector in 0u32..(16 * 32) {
            let (leaf, _) = vector_leaf_bit(vector);

            assert_eq!(vector_subtree_mask(vector), 1u32 << (leaf / 2));
        }

        assert_eq!(vector_subtree_mask(155), 1 << 2);
        assert_eq!(vector_subtree_mask(129), vector_subtree_mask(155));
    }

    /// [`Tree::new`] drops subtrees the architecture does not implement, so a caller cannot enable
    /// a `TOP` bit with no leaves behind it.
    #[test]
    fn tree_new_masks_unimplemented_subtrees() {
        assert_eq!(
            Tree::new(Chipset::TU102, IrqType::Msi, 0xff).serviced_subtrees,
            0x0f
        );
        assert_eq!(
            Tree::new(Chipset::GH100, IrqType::Msi, 0xff).serviced_subtrees,
            0xff
        );
    }

    /// Every supported chipset implements the subtree that carries the GSP notification.
    #[test]
    fn serviced_subtree_is_implemented_everywhere() {
        let serviced = crate::irq::gsp::GSP_SUBTREE;

        for chipset in [
            Chipset::TU102,
            Chipset::GA102,
            Chipset::AD102,
            Chipset::GH100,
            Chipset::GB100,
            Chipset::GB202,
        ] {
            assert_eq!(
                serviced & !cpu_interrupt_hal(chipset).implemented_subtrees(),
                0
            );
        }
    }
}
