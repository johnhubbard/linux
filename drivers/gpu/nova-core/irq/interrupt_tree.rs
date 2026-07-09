// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The GIN CPU interrupt tree for one PCIe function.
//!
//! A vector's number fixes where it latches: leaf `vector / 32` at bit `vector % 32`, and that
//! leaf belongs to subtree `vector / 64`. The types here keep those three views apart, so a leaf
//! index, a set of vectors within one leaf, and a `TOP` bit cannot stand in for one another.
//!
//! Servicing a leaf has a required order: read its pending bits, then clear them. Clearing a leaf
//! before reading it discards every vector latched in it, and nothing reports the loss. Only
//! [`Tree::read_pending`] produces a [`LeafPending`], and only a [`LeafPending`] can clear, so the
//! wrong order does not compile.
//!
//! Serializing access to the tree is the caller's responsibility.

use kernel::{
    io::{
        register::Array,
        Io, //
    },
    num::Bounded,
    pci::IrqType,
    prelude::*, //
};

use crate::{
    driver::Bar0,
    gpu::Chipset, //
};

use super::{
    hal::{
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
    }, //
};

/// Index of a leaf register, bounded to the `0..16` range covered by the leaf register arrays.
pub(super) type LeafIndex = Bounded<usize, 4>;

/// Number of vectors one leaf register carries, one per bit.
const VECTORS_PER_LEAF: u32 = 32;

/// Number of leaves one subtree covers.
const LEAVES_PER_SUBTREE: u32 = 2;

/// Mask that bounds a leaf index to the leaf register arrays.
const LEAF_INDEX_MASK: usize = LeafCount::Sixteen.into_raw() - 1;

/// Number of leaves a tree implements.
///
/// Every supported part implements one of these two counts, and the interrupt HAL names the one
/// its architecture uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(super) enum LeafCount {
    /// Turing through Ada.
    Eight = 8,

    /// Hopper and later.
    Sixteen = 16,
}

impl LeafCount {
    /// Returns the number of leaves.
    pub(super) const fn into_raw(self) -> usize {
        self as usize
    }

    /// Returns the number of subtrees, each of which covers two leaves.
    pub(super) const fn subtree_count(self) -> u32 {
        self as u32 / LEAVES_PER_SUBTREE
    }

    /// Returns the set of every subtree a tree of this size implements.
    pub(super) const fn subtree_set(self) -> SubtreeSet {
        SubtreeSet((1u32 << self.subtree_count()) - 1)
    }

    /// Returns the number of vectors a tree of this size carries.
    pub(super) const fn vector_count(self) -> u32 {
        self as u32 * VECTORS_PER_LEAF
    }
}

/// Set of vectors within one leaf, one bit per vector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LeafMask(u32);

impl LeafMask {
    /// Returns the mask with every vector of the leaf set.
    pub(super) const fn all() -> Self {
        Self(u32::MAX)
    }

    /// Returns the mask holding the vectors set in `raw`.
    pub(super) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the mask as the value the leaf registers take.
    pub(super) const fn into_raw(self) -> u32 {
        self.0
    }

    /// Returns whether no vector is set.
    pub(super) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns whether every vector set in `other` is also set here.
    pub(super) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One subtree, named by its `TOP` bit.
///
/// # Invariants
///
/// Exactly one bit is set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Subtree(u32);

impl Subtree {
    /// Returns this subtree's index within the tree.
    ///
    /// Under MSI-X this is also the index of the allocated entry the subtree raises.
    pub(super) const fn index(self) -> u32 {
        self.0.trailing_zeros()
    }

    /// Returns the subtree as the value the `TOP` enable registers take.
    pub(super) const fn into_raw(self) -> u32 {
        self.0
    }
}

/// Set of subtrees, one bit per subtree, in the layout the `TOP` enable registers take.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubtreeSet(u32);

impl SubtreeSet {
    /// Returns whether `subtree` belongs to this set.
    pub(super) const fn contains(self, subtree: Subtree) -> bool {
        self.0 & subtree.into_raw() != 0
    }

    /// Returns whether the set holds no subtree.
    pub(super) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns the subtrees present in both sets.
    pub(super) const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns the number of subtrees counted from subtree `0` through the highest one in this
    /// set, which is `0` for an empty set.
    pub(super) const fn span(self) -> u32 {
        u32::BITS - self.0.leading_zeros()
    }

    /// Returns the set as the value the `TOP` enable registers take.
    pub(super) const fn into_raw(self) -> u32 {
        self.0
    }
}

impl From<Subtree> for SubtreeSet {
    fn from(subtree: Subtree) -> Self {
        Self(subtree.into_raw())
    }
}

/// A GIN interrupt vector.
///
/// # Invariants
///
/// The vector lies within the widest tree any supported part implements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GinVector(u32);

impl GinVector {
    /// Returns the vector numbered `VECTOR`.
    ///
    /// Fails at build time if `VECTOR` lies outside the widest tree any supported part
    /// implements.
    pub(super) const fn new<const VECTOR: u32>() -> Self {
        build_assert!(VECTOR < LeafCount::Sixteen.vector_count());

        // INVARIANT: `VECTOR` is within the widest supported tree.
        Self(VECTOR)
    }

    /// Returns the vector number.
    pub(super) const fn into_raw(self) -> u32 {
        self.0
    }

    /// Returns the leaf that carries this vector.
    pub(super) fn leaf_index(self) -> LeafIndex {
        // By the type invariant the quotient is already below 16, so the mask changes nothing. It
        // is what proves the bound to `from_expr`.
        LeafIndex::from_expr(crate::num::u32_as_usize(self.0 / VECTORS_PER_LEAF) & LEAF_INDEX_MASK)
    }

    /// Returns this vector's bit within its leaf.
    pub(super) const fn leaf_mask(self) -> LeafMask {
        LeafMask(1 << (self.0 % VECTORS_PER_LEAF))
    }

    /// Returns the subtree that carries this vector.
    pub(super) const fn subtree(self) -> Subtree {
        // INVARIANT: a shift of `1` leaves exactly one bit set.
        Subtree(1 << (self.0 / (VECTORS_PER_LEAF * LEAVES_PER_SUBTREE)))
    }

    /// Checks that this vector lies within a tree of `leaves` leaves.
    ///
    /// # Errors
    ///
    /// `EINVAL` if the vector lies beyond the last leaf such a tree implements.
    pub(super) fn validate(self, leaves: LeafCount) -> Result {
        if self.0 >= leaves.vector_count() {
            return Err(EINVAL);
        }

        Ok(())
    }
}

/// Clears the enables of the vectors set in `vectors` for `leaf` (`LEAF_EN_CLEAR`).
///
/// Shared by [`Tree::disable_leaf`] and by [`LeafEnableGuard`]'s [`Drop`], which has no tree to
/// reach through.
fn clear_leaf_enables(bar: Bar0<'_>, leaf: LeafIndex, vectors: LeafMask) {
    if let Some(loc) = CPU_INTR_LEAF_EN_CLEAR::try_at(leaf.get()) {
        bar.write(loc, vectors.into_raw().into());
    }
}

/// Clears the `TOP` enables of every subtree in `serviced` (`TOP_EN_CLEAR`).
fn clear_top_enables(bar: Bar0<'_>, serviced: SubtreeSet) {
    bar.write(CPU_INTR_TOP_EN_CLEAR, serviced.into_raw().into());
}

/// Clears the pending vectors set in `vectors` for `leaf` (write-1-to-clear).
fn clear_leaf_pending(bar: Bar0<'_>, leaf: LeafIndex, vectors: LeafMask) {
    if !vectors.is_empty() {
        if let Some(loc) = CPU_INTR_LEAF::try_at(leaf.get()) {
            bar.write(loc, vectors.into_raw().into());
        }
    }
}

/// Returns the leaves that subtree `index` covers.
///
/// An index beyond the leaf register arrays yields nothing rather than panicking.
fn subtree_leaves(index: u32) -> impl Iterator<Item = LeafIndex> {
    let first = index * LEAVES_PER_SUBTREE;

    (first..first + LEAVES_PER_SUBTREE)
        .filter_map(|leaf| LeafIndex::try_new(crate::num::u32_as_usize(leaf)))
}

/// The GIN CPU interrupt tree for a single PCIe function.
///
/// Copying one is copying a borrowed BAR pointer and three small values, which an interrupt
/// handler needs so that it owns a tree of its own.
#[derive(Clone, Copy)]
pub(super) struct Tree<'a> {
    /// Borrowed BAR0, through which every tree register is reached.
    bar: Bar0<'a>,
    /// Number of leaves this tree implements.
    leaves: LeafCount,
    /// The subtrees this tree enables and services.
    serviced: SubtreeSet,
    /// Method that rearms PCI interrupt delivery, or `None` if the interrupt type needs no rearm
    /// write.
    rearm: Option<PciIrqRearmMethod>,
}

impl<'a> Tree<'a> {
    /// Creates a `Tree` for `chipset` covering `serviced`, with the rearm method that `irq_type`
    /// requires.
    ///
    /// Each serviced subtree must have an allocated PCI vector and a registered handler, which
    /// [`super::alloc_vectors`] sizes the allocation for. Subtrees the architecture does not
    /// implement are dropped.
    pub(super) fn new(
        bar: Bar0<'a>,
        chipset: Chipset,
        irq_type: IrqType,
        serviced: SubtreeSet,
    ) -> Self {
        let hal = cpu_interrupt_hal(chipset);
        let leaves = hal.leaf_count();

        Self {
            bar,
            leaves,
            serviced: serviced.intersection(leaves.subtree_set()),
            rearm: hal.pci_irq_rearm_method(irq_type),
        }
    }

    /// Rearms PCI interrupt delivery to the CPU after servicing `subtree`, the one subtree the
    /// calling handler serves.
    ///
    /// A handler must call this before it returns, or it receives no further interrupts.
    pub(super) fn rearm_pci_irq(&self, subtree: Subtree) {
        if let Some(method) = self.rearm {
            method.rearm(self.bar, self.serviced, subtree);
        }
    }

    /// Enables this tree's serviced subtrees (`TOP_EN_SET`).
    pub(super) fn enable_top(&self) {
        self.bar
            .write(CPU_INTR_TOP_EN_SET, self.serviced.into_raw().into());
    }

    /// Disables this tree's serviced subtrees (`TOP_EN_CLEAR`).
    pub(super) fn disable_top(&self) {
        clear_top_enables(self.bar, self.serviced);
    }

    /// Enables this tree's serviced subtrees until the returned guard drops.
    pub(super) fn enable_top_guarded(&self) -> TopEnableGuard<'a> {
        self.enable_top();

        TopEnableGuard {
            bar: self.bar,
            serviced: self.serviced,
        }
    }

    /// Enables the vectors set in `vectors` for `leaf` (`LEAF_EN_SET`).
    ///
    /// This is the per-vector counterpart of [`Self::enable_top`], which enables whole subtrees.
    pub(super) fn enable_leaf(&self, leaf: LeafIndex, vectors: LeafMask) {
        if let Some(loc) = CPU_INTR_LEAF_EN_SET::try_at(leaf.get()) {
            self.bar.write(loc, vectors.into_raw().into());
        }
    }

    /// Disables the vectors set in `vectors` for `leaf` (`LEAF_EN_CLEAR`).
    pub(super) fn disable_leaf(&self, leaf: LeafIndex, vectors: LeafMask) {
        clear_leaf_enables(self.bar, leaf, vectors);
    }

    /// Enables `vectors` for `leaf` until the returned guard drops.
    pub(super) fn enable_leaf_guarded(
        &self,
        leaf: LeafIndex,
        vectors: LeafMask,
    ) -> LeafEnableGuard<'a> {
        self.enable_leaf(leaf, vectors);

        LeafEnableGuard {
            bar: self.bar,
            leaf,
            vectors,
        }
    }

    /// Reads the vectors pending in `leaf`.
    pub(super) fn read_pending(&self, leaf: LeafIndex) -> LeafPending<'a> {
        let pending = CPU_INTR_LEAF::try_at(leaf.get())
            .map(|loc| self.bar.read(loc).into_raw())
            .unwrap_or(0);

        LeafPending {
            bar: self.bar,
            leaf,
            pending: LeafMask::from_raw(pending),
        }
    }

    /// Injects a software interrupt for `vector` via the trigger register.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `vector` lies outside this tree. `EOVERFLOW` if `vector` does not fit in the
    /// trigger register's vector field.
    // Only the interrupt self-test injects a software interrupt.
    #[cfg_attr(not(CONFIG_NOVA_CORE_IRQ_SELFTEST), expect(dead_code))]
    pub(super) fn trigger(&self, vector: GinVector) -> Result {
        vector.validate(self.leaves)?;
        self.bar
            .write_reg(CPU_INTR_LEAF_TRIGGER::zeroed().try_with_vector(vector.into_raw())?);

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
    pub(super) fn disable_all_leaves(&self) {
        for index in 0..self.leaves.into_raw() {
            if let Some(leaf) = LeafIndex::try_new(index) {
                self.disable_leaf(leaf, LeafMask::all());
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
    pub(super) fn drain(&self) {
        self.disable_top();

        // `TOP` summarizes enabled leaf bits, so a vector that latched while it was disabled does
        // not appear there.
        for index in 0..self.leaves.subtree_count() {
            for leaf in subtree_leaves(index) {
                let pending = self.read_pending(leaf);
                if !pending.vectors().is_empty() {
                    pending.clear();
                }
            }
        }

        self.enable_top();
    }
}

/// The vectors read pending from one leaf.
///
/// Holding one is the proof that the leaf was read, which is what [`Self::clear`] and
/// [`Self::clear_vectors`] require.
pub(super) struct LeafPending<'a> {
    bar: Bar0<'a>,
    leaf: LeafIndex,
    pending: LeafMask,
}

impl LeafPending<'_> {
    /// Returns the vectors that were pending.
    pub(super) fn vectors(&self) -> LeafMask {
        self.pending
    }

    /// Clears every vector that was pending, by writing its bits back (write-1-to-clear).
    pub(super) fn clear(&self) {
        self.clear_vectors(self.pending);
    }

    /// Clears the vectors set in `vectors` (write-1-to-clear), leaving every other pending bit
    /// set.
    ///
    /// A handler that services one vector uses this rather than [`Self::clear`], which clears
    /// every vector the leaf had pending.
    pub(super) fn clear_vectors(&self, vectors: LeafMask) {
        clear_leaf_pending(self.bar, self.leaf, vectors);
    }
}

/// Keeps a leaf's vectors enabled for as long as it is held.
///
/// Dropping it disables the same vectors, so an error path cannot leave a source enabled with no
/// handler behind it.
pub(super) struct LeafEnableGuard<'a> {
    bar: Bar0<'a>,
    leaf: LeafIndex,
    vectors: LeafMask,
}

impl Drop for LeafEnableGuard<'_> {
    fn drop(&mut self) {
        clear_leaf_enables(self.bar, self.leaf, self.vectors);
    }
}

/// Keeps a tree's serviced subtrees enabled at `TOP` for as long as it is held.
pub(super) struct TopEnableGuard<'a> {
    bar: Bar0<'a>,
    serviced: SubtreeSet,
}

impl Drop for TopEnableGuard<'_> {
    fn drop(&mut self) {
        clear_top_enables(self.bar, self.serviced);
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

    /// A leaf count yields one subtree per pair of leaves, and 32 vectors per leaf.
    #[test]
    fn leaf_count_derives_subtrees_and_vectors() {
        assert_eq!(LeafCount::Eight.subtree_count(), 4);
        assert_eq!(LeafCount::Eight.subtree_set().into_raw(), 0x0f);
        assert_eq!(LeafCount::Eight.vector_count(), 256);

        assert_eq!(LeafCount::Sixteen.subtree_count(), 8);
        assert_eq!(LeafCount::Sixteen.subtree_set().into_raw(), 0xff);
        assert_eq!(LeafCount::Sixteen.vector_count(), 512);
    }

    /// Subtree `N` covers the two adjacent leaves `2N` and `2N + 1`, and an index past the leaf
    /// register arrays yields nothing.
    #[test]
    fn subtree_covers_two_adjacent_leaves() {
        for index in 0..8u32 {
            let first = crate::num::u32_as_usize(index) * 2;
            let mut leaves = subtree_leaves(index);

            assert_eq!(leaves.next().map(LeafIndex::get), Some(first));
            assert_eq!(leaves.next().map(LeafIndex::get), Some(first + 1));
            assert!(leaves.next().is_none());
        }

        // Subtree 8 would cover leaves 16 and 17, both beyond the leaf index range.
        assert!(subtree_leaves(8).next().is_none());
    }

    /// A vector maps to its leaf, its bit within that leaf, and its subtree. The fixed doorbell
    /// (129) and GSP (155) vectors share a subtree, so one allocation and one enabled subtree
    /// serve both.
    #[test]
    fn vector_maps_to_leaf_bit_and_subtree() {
        let doorbell = GinVector::new::<129>();
        let gsp = GinVector::new::<155>();

        assert_eq!(doorbell.leaf_index().get(), 4);
        assert_eq!(doorbell.leaf_mask().into_raw(), 1 << 1);
        assert_eq!(doorbell.subtree().index(), 2);

        assert_eq!(gsp.leaf_index().get(), 4);
        assert_eq!(gsp.leaf_mask().into_raw(), 1 << 27);
        assert_eq!(gsp.subtree().index(), 2);

        assert_eq!(doorbell.subtree(), gsp.subtree());
    }

    /// Both fixed vectors lie within the 8-leaf tree, so every supported part carries them.
    #[test]
    fn fixed_vectors_fit_the_narrowest_tree() {
        assert!(GinVector::new::<129>().validate(LeafCount::Eight).is_ok());
        assert!(GinVector::new::<155>().validate(LeafCount::Eight).is_ok());

        // The first vector beyond an 8-leaf tree.
        assert!(GinVector::new::<256>().validate(LeafCount::Eight).is_err());
        assert!(GinVector::new::<256>().validate(LeafCount::Sixteen).is_ok());
    }

    /// A subtree set reports membership, intersection, and how far it extends from subtree 0.
    #[test]
    fn subtree_set_operations() {
        let gsp = GinVector::new::<155>().subtree();

        assert!(LeafCount::Eight.subtree_set().contains(gsp));
        assert!(!LeafCount::Eight.subtree_set().is_empty());

        // Subtree 2 is the highest the GSP needs, so an MSI-X request covers entries 0 through 2.
        assert_eq!(SubtreeSet::from(gsp).span(), 3);

        // Hopper implements every subtree an 8-leaf tree does.
        assert_eq!(
            LeafCount::Sixteen
                .subtree_set()
                .intersection(LeafCount::Eight.subtree_set()),
            LeafCount::Eight.subtree_set()
        );
    }

    /// Every supported chipset implements the subtree that carries the GSP notification.
    #[test]
    fn gsp_subtree_is_implemented_everywhere() {
        for chipset in [
            Chipset::TU102,
            Chipset::GA102,
            Chipset::AD102,
            Chipset::GH100,
            Chipset::GB100,
            Chipset::GB202,
        ] {
            assert!(cpu_interrupt_hal(chipset)
                .leaf_count()
                .subtree_set()
                .contains(crate::irq::gsp::GSP_SUBTREE));
        }
    }
}
