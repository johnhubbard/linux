// SPDX-License-Identifier: GPL-2.0

//! Macro to define register layout and accessors.
//!
//! The [`register!`](kernel::io::register!) macro provides an intuitive and readable syntax for
//! defining a dedicated type for each register and accessing it using [`Io`](super::Io). Each such
//! type comes with its own field accessors that can return an error if a field's value is invalid.
//!
//! Note: most of the items in this module are public only so the `register!` macro can name them
//! in its expansion. You rarely write any of these types yourself. The items you do interact with
//! are:
//!
//! - [`RegisterBase`]: implement this on your own (usually zero-sized) types to provide the base
//!   address of a group of relative registers. This is the one trait you write impls for.
//! - [`WithBase`] and [`Array`]: bring these into scope so you can call [`WithBase::of`] and
//!   [`Array::at`] to build the location of a relative or array register.
//!
//! Everything else (the `Register*` marker traits and the `*Loc` location structs) is generated
//! and named by the macro. See [Generated types](#generated-types) below for a map from each
//! declaration form to the traits and location type it produces.
//!
//! # Simple example
//!
//! ```no_run
//! use kernel::io::register;
//!
//! register! {
//!     /// Basic information about the chip.
//!     pub BOOT_0(u32) @ 0x00000100 {
//!         /// Vendor ID.
//!         15:8 vendor_id;
//!         /// Major revision of the chip.
//!         7:4 major_revision;
//!         /// Minor revision of the chip.
//!         3:0 minor_revision;
//!     }
//! }
//! ```
//!
//! This defines a 32-bit `BOOT_0` type which can be read from or written to offset `0x100` of an
//! `Io` region, with the described bitfields. For instance, `minor_revision` consists of the 4
//! least significant bits of the type.
//!
//! Fields are instances of [`Bounded`](kernel::num::Bounded) and can be read by calling their
//! getter method, which is named after them. They also have setter methods prefixed with `with_`
//! for runtime values and `with_const_` for constant values. All setters return the updated
//! register value.
//!
//! Fields can also be transparently converted from/to an arbitrary type by using the `=>` and
//! `?=>` syntaxes.
//!
//! If present, doc comments above register or fields definitions are added to the relevant item
//! they document (the register type itself, or the field's setter and getter methods).
//!
//! Note that multiple registers can be defined in a single `register!` invocation. This can be
//! useful to group related registers together.
//!
//! Here is how the register defined above can be used in code:
//!
//!
//! ```no_run
//! use kernel::{
//!     io::{
//!         register,
//!         Io,
//!         IoLoc,
//!     },
//!     num::Bounded,
//! };
//! # use kernel::io::Mmio;
//! # register! {
//! #     pub BOOT_0(u32) @ 0x00000100 {
//! #         15:8 vendor_id;
//! #         7:4 major_revision;
//! #         3:0 minor_revision;
//! #     }
//! # }
//! # fn test(io: &Mmio<0x1000>) {
//! # fn obtain_vendor_id() -> u8 { 0xff }
//!
//! // Read from the register's defined offset (0x100).
//! let boot0 = io.read(BOOT_0);
//! pr_info!("chip revision: {}.{}", boot0.major_revision().get(), boot0.minor_revision().get());
//!
//! // Update some fields and write the new value back.
//! let new_boot0 = boot0
//!     // Constant values.
//!     .with_const_major_revision::<3>()
//!     .with_const_minor_revision::<10>()
//!     // Runtime value.
//!     .with_vendor_id(obtain_vendor_id());
//! io.write_reg(new_boot0);
//!
//! // Or, build a new value from zero and write it:
//! io.write_reg(BOOT_0::zeroed()
//!     .with_const_major_revision::<3>()
//!     .with_const_minor_revision::<10>()
//!     .with_vendor_id(obtain_vendor_id())
//! );
//!
//! // Or, read and update the register in a single step.
//! io.update(BOOT_0, |r| r
//!     .with_const_major_revision::<3>()
//!     .with_const_minor_revision::<10>()
//!     .with_vendor_id(obtain_vendor_id())
//! );
//!
//! // Constant values can also be built using the const setters.
//! const V: BOOT_0 = pin_init::zeroed::<BOOT_0>()
//!     .with_const_major_revision::<3>()
//!     .with_const_minor_revision::<10>();
//! # }
//! ```
//!
//! # Generated types
//!
//! Each `register!` declaration expands into a dedicated register type (named after the register,
//! carrying the bitfield accessors) plus a set of trait impls and a way to build a *location*. A
//! location is anything that implements [`IoLoc`](super::IoLoc), which is what
//! [`Io::read`](super::Io::read), [`Io::write`](super::Io::write), and
//! [`Io::update`](super::Io::update) consume. The data flow is:
//!
//! ```text
//! register! { NAME ... }  -->  NAME (bitfields)  +  location builder  -->  IoLoc  -->  Io::read/write
//! ```
//!
//! Which traits get implemented, and how you spell the location, depends on the declaration form:
//!
//! ```text
//! Declaration form                    Traits implemented on NAME       Location is built with
//! ----------------------------------  -------------------------------  --------------------------
//! NAME(u32) @ 0x100                   Register, FixedRegister          NAME           (or `()`)
//! NAME(u32) @ Base + 0x10             Register, WithBase,              NAME::of::<B>()
//!                                       RelativeRegister
//! NAME(u32)[N] @ 0x100                Register, RegisterArray, Array   NAME::at(i)
//! NAME(u32)[N] @ Base + 0x10          Register, WithBase,              NAME::of::<B>().at(i)
//!                                       RegisterArray,
//!                                       RelativeRegisterArray
//! ```
//!
//! In the relative forms, `Base` is one of your own types and `B` is a type implementing
//! [`RegisterBase<Base>`](RegisterBase) (see the [relative registers
//! section](kernel::io::register!#relative-registers) of the macro docs). The corresponding
//! location structs ([`FixedRegisterLoc`], [`RelativeRegisterLoc`], [`RegisterArrayLoc`],
//! [`RelativeRegisterArrayLoc`]) are produced by the builders above and rarely named by hand.
//!
//! For more extensive documentation about how to define registers, see the
//! [`register!`](kernel::io::register!) macro.

use core::marker::PhantomData;

use crate::io::IoLoc;

use kernel::build_assert;

/// Trait implemented by every type generated by [`register!`](kernel::io::register!).
///
/// It ties a register type to its backing primitive ([`Storage`](Register::Storage), e.g. `u32`)
/// and its [`OFFSET`](Register::OFFSET). The macro implements this for you, and you never name it
/// except in trait bounds. The other `Register*` traits in this module refine it to describe how
/// the offset is interpreted.
pub trait Register: Sized {
    /// Backing primitive type of the register.
    type Storage: Into<Self> + From<Self>;

    /// Start offset of the register.
    ///
    /// The interpretation of this offset depends on the type of the register.
    const OFFSET: usize;
}

/// Marker trait for registers whose [`OFFSET`](Register::OFFSET) is an absolute offset into the
/// I/O region.
///
/// Generated by the `NAME(u32) @ 0x100` declaration form. You never implement this. Its location
/// is the macro-generated `NAME` constant (a [`FixedRegisterLoc`]), and `()` can stand in for it
/// when writing a value back, since the offset already lives in the type.
pub trait FixedRegister: Register {}

/// Allows `()` to be used as the `location` parameter of [`Io::write`](super::Io::write) when
/// passing a [`FixedRegister`] value.
impl<T> IoLoc<T> for ()
where
    T: FixedRegister,
{
    type IoType = T::Storage;

    #[inline(always)]
    fn offset(self) -> usize {
        T::OFFSET
    }
}

/// A [`FixedRegister`] carries its location in its type. Thus `FixedRegister` values can be used
/// as an [`IoLoc`].
impl<T> IoLoc<T> for T
where
    T: FixedRegister,
{
    type IoType = T::Storage;

    #[inline(always)]
    fn offset(self) -> usize {
        T::OFFSET
    }
}

/// Location of a [`FixedRegister`], and the type of the macro-generated `NAME` constant.
///
/// You rarely name this type: referencing the register by its name yields one, and it implements
/// [`IoLoc`] so it can be passed straight to [`Io::read`](super::Io::read) and friends.
pub struct FixedRegisterLoc<T: FixedRegister>(PhantomData<T>);

impl<T: FixedRegister> FixedRegisterLoc<T> {
    /// Returns the location of `T`.
    #[inline(always)]
    // We do not implement `Default` so we can be const.
    #[expect(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> IoLoc<T> for FixedRegisterLoc<T>
where
    T: FixedRegister,
{
    type IoType = T::Storage;

    #[inline(always)]
    fn offset(self) -> usize {
        T::OFFSET
    }
}

/// Provides the base address added to a relative register's offset to obtain its absolute offset.
///
/// This is the one trait in this module you implement yourself. For each relative register
/// declared as `@ Base + offset`, `Base` names a *base family*, and you implement
/// `RegisterBase<Base>` on each type that can act as a base for that family, supplying the
/// concrete [`BASE`](RegisterBase::BASE) address. The implementor is typically a zero-sized type
/// that exists only to carry this constant.
///
/// The `T` parameter is the base family. It lets one type provide several independent bases (by
/// implementing `RegisterBase` once per family) and lets the macro restrict a register to the
/// implementors of its own family, so an unrelated base cannot be passed by mistake.
///
/// # Examples
///
/// ```ignore
/// // Base family declared as the `Base` in a relative `register!` declaration.
/// pub struct CpuCtlBase;
///
/// // Zero-sized type that provides the base address for that family.
/// struct Cpu0;
/// impl RegisterBase<CpuCtlBase> for Cpu0 {
///     const BASE: usize = 0x100;
/// }
/// ```
///
/// See the "Relative registers" section of the [`register!`](kernel::io::register!) macro docs
/// for a complete, runnable example.
pub trait RegisterBase<T> {
    /// Base address to which register offsets are added.
    const BASE: usize;
}

/// Trait implemented by every relative register and relative register array, providing the
/// [`of`](WithBase::of) location builder.
///
/// Generated by the `@ Base + 0x10` declaration forms. You never implement this, but you bring it
/// into scope to call [`of`](WithBase::of) (either as `NAME::of::<B>()` or `WithBase::of::<B>()`)
/// and select which base `B` to resolve the register against.
pub trait WithBase {
    /// Family of bases applicable to this register.
    type BaseFamily;

    /// Returns the absolute location of this type when using `B` as its base.
    #[inline(always)]
    fn of<B: RegisterBase<Self::BaseFamily>>() -> RelativeRegisterLoc<Self, B>
    where
        Self: Register,
    {
        RelativeRegisterLoc::new()
    }
}

/// Marker trait for registers whose offset is relative to a base address.
///
/// Generated by the `NAME(u32) @ Base + 0x10` declaration form. You never implement this. Build
/// its location with [`NAME::of::<B>()`](WithBase::of), where `B` implements
/// [`RegisterBase<Base>`](RegisterBase).
pub trait RelativeRegister: Register + WithBase {}

/// Location of a relative register, produced by [`WithBase::of`].
///
/// You rarely name this type. For a plain [`RelativeRegister`] it already implements [`IoLoc`] and
/// is ready to pass to [`Io::read`](super::Io::read). For a [`RelativeRegisterArray`] it needs one
/// more step, [`at`](RelativeRegisterLoc::at), to select the index before it can be used.
pub struct RelativeRegisterLoc<T: WithBase, B: ?Sized>(PhantomData<T>, PhantomData<B>);

impl<T, B> RelativeRegisterLoc<T, B>
where
    T: Register + WithBase,
    B: RegisterBase<T::BaseFamily> + ?Sized,
{
    /// Returns the location of a relative register or register array.
    #[inline(always)]
    // We do not implement `Default` so we can be const.
    #[expect(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self(PhantomData, PhantomData)
    }

    // Returns the absolute offset of the relative register using base `B`.
    //
    // This is implemented as a private const method so it can be reused by the [`IoLoc`]
    // implementations of both [`RelativeRegisterLoc`] and [`RelativeRegisterArrayLoc`].
    #[inline]
    const fn offset(self) -> usize {
        B::BASE + T::OFFSET
    }
}

impl<T, B> IoLoc<T> for RelativeRegisterLoc<T, B>
where
    T: RelativeRegister,
    B: RegisterBase<T::BaseFamily> + ?Sized,
{
    type IoType = T::Storage;

    #[inline(always)]
    fn offset(self) -> usize {
        RelativeRegisterLoc::offset(self)
    }
}

/// Marker trait for registers declared as an array, providing the array's
/// [`SIZE`](RegisterArray::SIZE) and [`STRIDE`](RegisterArray::STRIDE).
///
/// Generated by the `NAME(u32)[N] @ ...` declaration forms. You never implement this. Index into
/// the array with [`Array::at`] (or [`Array::try_at`]) to build a [`RegisterArrayLoc`].
pub trait RegisterArray: Register {
    /// Number of elements in the registers array.
    const SIZE: usize;
    /// Number of bytes between the start of elements in the registers array.
    const STRIDE: usize;
}

/// Location of one element of a [`RegisterArray`], produced by [`Array::at`] or [`Array::try_at`].
///
/// You rarely name this type. It carries the chosen index and implements [`IoLoc`], so it can be
/// passed straight to [`Io::read`](super::Io::read) and friends.
pub struct RegisterArrayLoc<T: RegisterArray>(usize, PhantomData<T>);

impl<T: RegisterArray> RegisterArrayLoc<T> {
    /// Returns the location of register `T` at position `idx`, with build-time validation.
    #[inline(always)]
    pub fn new(idx: usize) -> Self {
        build_assert!(idx < T::SIZE);

        Self(idx, PhantomData)
    }

    /// Attempts to return the location of register `T` at position `idx`, with runtime validation.
    #[inline(always)]
    pub fn try_new(idx: usize) -> Option<Self> {
        if idx < T::SIZE {
            Some(Self(idx, PhantomData))
        } else {
            None
        }
    }
}

impl<T> IoLoc<T> for RegisterArrayLoc<T>
where
    T: RegisterArray,
{
    type IoType = T::Storage;

    #[inline(always)]
    fn offset(self) -> usize {
        T::OFFSET + self.0 * T::STRIDE
    }
}

/// Provides the [`at`](Array::at) and [`try_at`](Array::try_at) location builders for
/// [`RegisterArray`]s.
///
/// Generated for every register array. You never implement this, but you bring it into scope to
/// call [`at`](Array::at) (either as `NAME::at(i)` or `Array::at(i)`) and index into the array.
pub trait Array {
    /// Returns the location of the register at position `idx`, with build-time validation.
    #[inline(always)]
    fn at(idx: usize) -> RegisterArrayLoc<Self>
    where
        Self: RegisterArray,
    {
        RegisterArrayLoc::new(idx)
    }

    /// Returns the location of the register at position `idx`, with runtime validation.
    #[inline(always)]
    fn try_at(idx: usize) -> Option<RegisterArrayLoc<Self>>
    where
        Self: RegisterArray,
    {
        RegisterArrayLoc::try_new(idx)
    }
}

/// Marker trait for register arrays whose offset is relative to a base address.
///
/// Generated by the `NAME(u32)[N] @ Base + 0x10` declaration form. You never implement this.
/// Build a location in two steps: [`NAME::of::<B>()`](WithBase::of) selects the base, then
/// [`at`](RelativeRegisterLoc::at) (or [`try_at`](RelativeRegisterLoc::try_at)) selects the index,
/// yielding a [`RelativeRegisterArrayLoc`].
pub trait RelativeRegisterArray: RegisterArray + WithBase {}

/// Location of one element of a [`RelativeRegisterArray`], produced by
/// [`NAME::of::<B>().at(i)`](RelativeRegisterLoc::at).
///
/// You rarely name this type. It pairs the resolved base with the chosen index and implements
/// [`IoLoc`], so it can be passed straight to [`Io::read`](super::Io::read) and friends.
pub struct RelativeRegisterArrayLoc<
    T: RelativeRegisterArray,
    B: RegisterBase<T::BaseFamily> + ?Sized,
>(RelativeRegisterLoc<T, B>, usize);

impl<T, B> RelativeRegisterArrayLoc<T, B>
where
    T: RelativeRegisterArray,
    B: RegisterBase<T::BaseFamily> + ?Sized,
{
    /// Returns the location of register `T` from the base `B` at index `idx`, with build-time
    /// validation.
    #[inline(always)]
    pub fn new(idx: usize) -> Self {
        build_assert!(idx < T::SIZE);

        Self(RelativeRegisterLoc::new(), idx)
    }

    /// Attempts to return the location of register `T` from the base `B` at index `idx`, with
    /// runtime validation.
    #[inline(always)]
    pub fn try_new(idx: usize) -> Option<Self> {
        if idx < T::SIZE {
            Some(Self(RelativeRegisterLoc::new(), idx))
        } else {
            None
        }
    }
}

/// Methods exclusive to [`RelativeRegisterLoc`]s created with a [`RelativeRegisterArray`].
impl<T, B> RelativeRegisterLoc<T, B>
where
    T: RelativeRegisterArray,
    B: RegisterBase<T::BaseFamily> + ?Sized,
{
    /// Returns the location of the register at position `idx`, with build-time validation.
    #[inline(always)]
    pub fn at(self, idx: usize) -> RelativeRegisterArrayLoc<T, B> {
        RelativeRegisterArrayLoc::new(idx)
    }

    /// Returns the location of the register at position `idx`, with runtime validation.
    #[inline(always)]
    pub fn try_at(self, idx: usize) -> Option<RelativeRegisterArrayLoc<T, B>> {
        RelativeRegisterArrayLoc::try_new(idx)
    }
}

impl<T, B> IoLoc<T> for RelativeRegisterArrayLoc<T, B>
where
    T: RelativeRegisterArray,
    B: RegisterBase<T::BaseFamily> + ?Sized,
{
    type IoType = T::Storage;

    #[inline(always)]
    fn offset(self) -> usize {
        self.0.offset() + self.1 * T::STRIDE
    }
}

/// Bundles a register value with the location at which to write it, so a single value carries
/// everything [`Io::write_reg`](super::Io::write_reg) needs.
///
/// This is what lets [`Io::write_reg`](super::Io::write_reg) take just one argument. A
/// [`FixedRegister`] value implements this directly (its location is in its type), so
/// `io.write_reg(NAME::zeroed()...)` works without naming a location. You do not implement this
/// yourself.
pub trait LocatedRegister {
    /// Register value to write.
    type Value: Register;
    /// Full location information at which to write the value.
    type Location: IoLoc<Self::Value>;

    /// Consumes `self` and returns a `(location, value)` tuple describing a valid I/O write
    /// operation.
    fn into_io_op(self) -> (Self::Location, Self::Value);
}

impl<T> LocatedRegister for T
where
    T: FixedRegister,
{
    type Location = FixedRegisterLoc<Self::Value>;
    type Value = T;

    #[inline(always)]
    fn into_io_op(self) -> (FixedRegisterLoc<T>, T) {
        (FixedRegisterLoc::new(), self)
    }
}

/// Defines a dedicated type for a register, including getter and setter methods for its fields and
/// methods to read and write it from an [`Io`](kernel::io::Io) region.
///
/// This documentation focuses on how to declare registers. See the [module-level
/// documentation](mod@kernel::io::register) for examples of how to access them.
///
/// There are 4 possible kinds of registers: fixed offset registers, relative registers, arrays of
/// registers, and relative arrays of registers.
///
/// ## Fixed offset registers
///
/// These are the simplest kind of registers. Their location is simply an offset inside the I/O
/// region. For instance:
///
/// ```ignore
/// register! {
///     pub FIXED_REG(u16) @ 0x80 {
///         ...
///     }
/// }
/// ```
///
/// This creates a 16-bit register named `FIXED_REG` located at offset `0x80` of an I/O region.
///
/// These registers' location can be built simply by referencing their name:
///
/// ```no_run
/// use kernel::{
///     io::{
///         register,
///         Io,
///     },
/// };
/// # use kernel::io::Mmio;
///
/// register! {
///     FIXED_REG(u32) @ 0x100 {
///         16:8 high_byte;
///         7:0  low_byte;
///     }
/// }
///
/// # fn test(io: &Mmio<0x1000>) {
/// let val = io.read(FIXED_REG);
///
/// // Write from an already-existing value.
/// io.write(FIXED_REG, val.with_low_byte(0xff));
///
/// // Create a register value from scratch.
/// let val2 = FIXED_REG::zeroed().with_high_byte(0x80);
///
/// // The location of fixed offset registers is already contained in their type. Thus, the
/// // `location` argument of `Io::write` is technically redundant and can be replaced by `()`.
/// io.write((), val2);
///
/// // Or, the single-argument `Io::write_reg` can be used.
/// io.write_reg(val2);
/// # }
///
/// ```
///
/// It is possible to create an alias of an existing register with new field definitions by using
/// the `=> ALIAS` syntax. This is useful for cases where a register's interpretation depends on
/// the context:
///
/// ```no_run
/// use kernel::io::register;
///
/// register! {
///     /// Scratch register.
///     pub SCRATCH(u32) @ 0x00000200 {
///         31:0 value;
///     }
///
///     /// Boot status of the firmware.
///     pub SCRATCH_BOOT_STATUS(u32) => SCRATCH {
///         0:0 completed;
///     }
/// }
/// ```
///
/// In this example, `SCRATCH_BOOT_STATUS` uses the same I/O address as `SCRATCH`, while providing
/// its own `completed` field.
///
/// ## Relative registers
///
/// Relative registers can be instantiated several times at a relative offset of a group of bases.
/// For instance, imagine the following I/O space:
///
/// ```text
///           +-----------------------------+
///           |             ...             |
///           |                             |
///  0x100--->+------------CPU0-------------+
///           |                             |
///  0x110--->+-----------------------------+
///           |           CPU_CTL           |
///           +-----------------------------+
///           |             ...             |
///           |                             |
///           |                             |
///  0x200--->+------------CPU1-------------+
///           |                             |
///  0x210--->+-----------------------------+
///           |           CPU_CTL           |
///           +-----------------------------+
///           |             ...             |
///           +-----------------------------+
/// ```
///
/// `CPU0` and `CPU1` both have a `CPU_CTL` register that starts at offset `0x10` of their I/O
/// space segment. Since both instances of `CPU_CTL` share the same layout, we don't want to define
/// them twice and would prefer a way to select which one to use from a single definition.
///
/// This can be done using the `Base + Offset` syntax when specifying the register's address:
///
/// ```ignore
/// register! {
///     pub RELATIVE_REG(u32) @ Base + 0x80 {
///         ...
///     }
/// }
/// ```
///
/// This creates a register with an offset of `0x80` from a given base.
///
/// `Base` is an arbitrary type (typically a ZST) to be used as a generic parameter of the
/// [`RegisterBase`] trait to provide the base as a constant, i.e. each type providing a base for
/// this register needs to implement `RegisterBase<Base>`.
///
/// The location of relative registers can be built using the [`WithBase::of`] method to specify
/// its base. All relative registers implement [`WithBase`].
///
/// Here is the above layout translated into code:
///
/// ```no_run
/// use kernel::{
///     io::{
///         register,
///         register::{
///             RegisterBase,
///             WithBase,
///         },
///         Io,
///     },
/// };
/// # use kernel::io::Mmio;
///
/// // Type used to identify the base.
/// pub struct CpuCtlBase;
///
/// // ZST describing `CPU0`.
/// struct Cpu0;
/// impl RegisterBase<CpuCtlBase> for Cpu0 {
///     const BASE: usize = 0x100;
/// }
///
/// // ZST describing `CPU1`.
/// struct Cpu1;
/// impl RegisterBase<CpuCtlBase> for Cpu1 {
///     const BASE: usize = 0x200;
/// }
///
/// // This makes `CPU_CTL` accessible from all implementors of `RegisterBase<CpuCtlBase>`.
/// register! {
///     /// CPU core control.
///     pub CPU_CTL(u32) @ CpuCtlBase + 0x10 {
///         0:0 start;
///     }
/// }
///
/// # fn test(io: Mmio<0x1000>) {
/// // Read the status of `Cpu0`.
/// let cpu0_started = io.read(CPU_CTL::of::<Cpu0>());
///
/// // Stop `Cpu0`.
/// io.write(WithBase::of::<Cpu0>(), CPU_CTL::zeroed());
/// # }
///
/// // Aliases can also be defined for relative register.
/// register! {
///     /// Alias to CPU core control.
///     pub CPU_CTL_ALIAS(u32) => CpuCtlBase + CPU_CTL {
///         /// Start the aliased CPU core.
///         1:1 alias_start;
///     }
/// }
///
/// # fn test2(io: Mmio<0x1000>) {
/// // Start the aliased `CPU0`, leaving its other fields untouched.
/// io.update(CPU_CTL_ALIAS::of::<Cpu0>(), |r| r.with_alias_start(true));
/// # }
/// ```
///
/// ## Arrays of registers
///
/// Some I/O areas contain consecutive registers that share the same field layout. These areas can
/// be defined as an array of identical registers, allowing them to be accessed by index with
/// compile-time or runtime bound checking:
///
/// ```ignore
/// register! {
///     pub REGISTER_ARRAY(u8)[10, stride = 4] @ 0x100 {
///         ...
///     }
/// }
/// ```
///
/// This defines `REGISTER_ARRAY`, an array of 10 byte registers starting at offset `0x100`. Each
/// register is separated from its neighbor by 4 bytes.
///
/// The `stride` parameter is optional; if unspecified, the registers are placed consecutively from
/// each other.
///
/// A location for a register in a register array is built using the [`Array::at`] trait method.
/// All arrays of registers implement [`Array`].
///
/// ```no_run
/// use kernel::{
///     io::{
///         register,
///         register::Array,
///         Io,
///     },
/// };
/// # use kernel::io::Mmio;
/// # fn get_scratch_idx() -> usize {
/// #   0x15
/// # }
///
/// // Array of 64 consecutive registers with the same layout starting at offset `0x80`.
/// register! {
///     /// Scratch registers.
///     pub SCRATCH(u32)[64] @ 0x00000080 {
///         31:0 value;
///     }
/// }
///
/// # fn test(io: &Mmio<0x1000>)
/// #     -> Result<(), Error>{
/// // Read scratch register 0, i.e. I/O address `0x80`.
/// let scratch_0 = io.read(SCRATCH::at(0)).value();
///
/// // Write scratch register 15, i.e. I/O address `0x80 + (15 * 4)`.
/// io.write(Array::at(15), SCRATCH::from(0xffeeaabb));
///
/// // This is out of bounds and won't build.
/// // let scratch_128 = io.read(SCRATCH::at(128)).value();
///
/// // Runtime-obtained array index.
/// let idx = get_scratch_idx();
/// // Access on a runtime index returns an error if it is out-of-bounds.
/// let some_scratch = io.read(SCRATCH::try_at(idx).ok_or(EINVAL)?).value();
///
/// // Alias to a specific register in an array.
/// // Here `SCRATCH[8]` is used to convey the firmware exit code.
/// register! {
///     /// Firmware exit status code.
///     pub FIRMWARE_STATUS(u32) => SCRATCH[8] {
///         7:0 status;
///     }
/// }
///
/// let status = io.read(FIRMWARE_STATUS).status();
///
/// // Non-contiguous register arrays can be defined by adding a stride parameter.
/// // Here, each of the 16 registers of the array is separated by 8 bytes, meaning that the
/// // registers of the two declarations below are interleaved.
/// register! {
///     /// Scratch registers bank 0.
///     pub SCRATCH_INTERLEAVED_0(u32)[16, stride = 8] @ 0x000000c0 {
///         31:0 value;
///     }
///
///     /// Scratch registers bank 1.
///     pub SCRATCH_INTERLEAVED_1(u32)[16, stride = 8] @ 0x000000c4 {
///         31:0 value;
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// ## Relative arrays of registers
///
/// Combining the two features described in the sections above, arrays of registers accessible from
/// a base can also be defined:
///
/// ```ignore
/// register! {
///     pub RELATIVE_REGISTER_ARRAY(u8)[10, stride = 4] @ Base + 0x100 {
///         ...
///     }
/// }
/// ```
///
/// Like relative registers, they implement the [`WithBase`] trait. However the return value of
/// [`WithBase::of`] cannot be used directly as a location and must be further specified using the
/// [`at`](RelativeRegisterLoc::at) method.
///
/// ```no_run
/// use kernel::{
///     io::{
///         register,
///         register::{
///             RegisterBase,
///             WithBase,
///         },
///         Io,
///     },
/// };
/// # use kernel::io::Mmio;
/// # fn get_scratch_idx() -> usize {
/// #   0x15
/// # }
///
/// // Type used as parameter of `RegisterBase` to specify the base.
/// pub struct CpuCtlBase;
///
/// // ZST describing `CPU0`.
/// struct Cpu0;
/// impl RegisterBase<CpuCtlBase> for Cpu0 {
///     const BASE: usize = 0x100;
/// }
///
/// // ZST describing `CPU1`.
/// struct Cpu1;
/// impl RegisterBase<CpuCtlBase> for Cpu1 {
///     const BASE: usize = 0x200;
/// }
///
/// // 64 per-cpu scratch registers, arranged as a contiguous array.
/// register! {
///     /// Per-CPU scratch registers.
///     pub CPU_SCRATCH(u32)[64] @ CpuCtlBase + 0x00000080 {
///         31:0 value;
///     }
/// }
///
/// # fn test(io: &Mmio<0x1000>) -> Result<(), Error> {
/// // Read scratch register 0 of CPU0.
/// let scratch = io.read(CPU_SCRATCH::of::<Cpu0>().at(0));
///
/// // Write the retrieved value into scratch register 15 of CPU1.
/// io.write(WithBase::of::<Cpu1>().at(15), scratch);
///
/// // This won't build.
/// // let cpu0_scratch_128 = io.read(CPU_SCRATCH::of::<Cpu0>().at(128)).value();
///
/// // Runtime-obtained array index.
/// let scratch_idx = get_scratch_idx();
/// // Access on a runtime index returns an error if it is out-of-bounds.
/// let cpu0_scratch = io.read(
///     CPU_SCRATCH::of::<Cpu0>().try_at(scratch_idx).ok_or(EINVAL)?
/// ).value();
/// # Ok(())
/// # }
///
/// // Alias to `SCRATCH[8]` used to convey the firmware exit code.
/// register! {
///     /// Per-CPU firmware exit status code.
///     pub CPU_FIRMWARE_STATUS(u32) => CpuCtlBase + CPU_SCRATCH[8] {
///         7:0 status;
///     }
/// }
///
/// // Non-contiguous relative register arrays can be defined by adding a stride parameter.
/// // Here, each of the 16 registers of the array is separated by 8 bytes, meaning that the
/// // registers of the two declarations below are interleaved.
/// register! {
///     /// Scratch registers bank 0.
///     pub CPU_SCRATCH_INTERLEAVED_0(u32)[16, stride = 8] @ CpuCtlBase + 0x00000d00 {
///         31:0 value;
///     }
///
///     /// Scratch registers bank 1.
///     pub CPU_SCRATCH_INTERLEAVED_1(u32)[16, stride = 8] @ CpuCtlBase + 0x00000d04 {
///         31:0 value;
///     }
/// }
///
/// # fn test2(io: &Mmio<0x1000>) -> Result<(), Error> {
/// let cpu0_status = io.read(CPU_FIRMWARE_STATUS::of::<Cpu0>()).status();
/// # Ok(())
/// # }
/// ```
#[macro_export]
macro_rules! register {
    // Internal structure of this macro, for readers who need to follow the expansion:
    //
    //   * The public entry rule (immediately below) matches every declaration form and forwards
    //     each register to one `@reg` rule.
    //   * One `@reg` rule per declaration form (fixed, relative, array, relative array, and their
    //     `=>` alias variants) picks the offset and dispatches to the shared helpers below.
    //   * `@bitfield*` helpers emit the `$name` type itself and its field getters and setters.
    //   * `@io_*` helpers emit the trait impls that classify the register: `@io_base` for
    //     `Register`, then `@io_fixed`, `@io_relative`, `@io_array`, or `@io_relative_array` for
    //     the form-specific markers and location builders.
    //
    // The traits and location types these helpers implement are documented at the module level.

    // Entry point for the macro, allowing multiple registers to be defined in one call.
    // It matches all possible register declaration patterns to dispatch them to corresponding
    // `@reg` rule that defines a single register.
    (
        $(
            $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty)
                $([ $size:expr $(, stride = $stride:expr)? ])?
                $(@ $($base:ident +)? $offset:literal)?
                $(=> $alias:ident $(+ $alias_offset:ident)? $([$alias_idx:expr])? )?
            { $($fields:tt)* }
        )*
    ) => {
        $(
        $crate::register!(
            @reg $(#[$attr])* $vis $name ($storage) $([$size $(, stride = $stride)?])?
                $(@ $($base +)? $offset)?
                $(=> $alias $(+ $alias_offset)? $([$alias_idx])? )?
            { $($fields)* }
        );
        )*
    };

    // All the rules below are private helpers.

    // Creates a register at a fixed offset of the MMIO space.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) @ $offset:literal
            { $($fields:tt)* }
    ) => {
        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(@io_base $name($storage) @ $offset);
        $crate::register!(@io_fixed $(#[$attr])* $vis $name($storage));
    };

    // Creates an alias register of fixed offset register `alias` with its own fields.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) => $alias:ident
            { $($fields:tt)* }
    ) => {
        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(
            @io_base $name($storage) @
            <$alias as $crate::io::register::Register>::OFFSET
        );
        $crate::register!(@io_fixed $(#[$attr])* $vis $name($storage));
    };

    // Creates a register at a relative offset from a base address provider.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) @ $base:ident + $offset:literal
            { $($fields:tt)* }
    ) => {
        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(@io_base $name($storage) @ $offset);
        $crate::register!(@io_relative $vis $name($storage) @ $base);
    };

    // Creates an alias register of relative offset register `alias` with its own fields.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) => $base:ident + $alias:ident
            { $($fields:tt)* }
    ) => {
        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(
            @io_base $name($storage) @ <$alias as $crate::io::register::Register>::OFFSET
        );
        $crate::register!(@io_relative $vis $name($storage) @ $base);
    };

    // Creates an array of registers at a fixed offset of the MMIO space.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty)
            [ $size:expr, stride = $stride:expr ] @ $offset:literal { $($fields:tt)* }
    ) => {
        ::kernel::static_assert!(::core::mem::size_of::<$storage>() <= $stride);

        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(@io_base $name($storage) @ $offset);
        $crate::register!(@io_array $vis $name($storage) [ $size, stride = $stride ]);
    };

    // Shortcut for contiguous array of registers (stride == size of element).
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) [ $size:expr ] @ $offset:literal
            { $($fields:tt)* }
    ) => {
        $crate::register!(
            $(#[$attr])* $vis $name($storage) [ $size, stride = ::core::mem::size_of::<$storage>() ]
                @ $offset { $($fields)* }
        );
    };

    // Creates an alias of register `idx` of array of registers `alias` with its own fields.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) => $alias:ident [ $idx:expr ]
            { $($fields:tt)* }
    ) => {
        ::kernel::static_assert!($idx < <$alias as $crate::io::register::RegisterArray>::SIZE);

        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(
            @io_base $name($storage) @
            <$alias as $crate::io::register::Register>::OFFSET
                + $idx * <$alias as $crate::io::register::RegisterArray>::STRIDE
        );
        $crate::register!(@io_fixed $(#[$attr])* $vis $name($storage));
    };

    // Creates an array of registers at a relative offset from a base address provider.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty)
            [ $size:expr, stride = $stride:expr ]
            @ $base:ident + $offset:literal { $($fields:tt)* }
    ) => {
        ::kernel::static_assert!(::core::mem::size_of::<$storage>() <= $stride);

        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(@io_base $name($storage) @ $offset);
        $crate::register!(
            @io_relative_array $vis $name($storage) [ $size, stride = $stride ] @ $base + $offset
        );
    };

    // Shortcut for contiguous array of relative registers (stride == size of element).
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty) [ $size:expr ]
            @ $base:ident + $offset:literal { $($fields:tt)* }
    ) => {
        $crate::register!(
            $(#[$attr])* $vis $name($storage) [ $size, stride = ::core::mem::size_of::<$storage>() ]
                @ $base + $offset { $($fields)* }
        );
    };

    // Creates an alias of register `idx` of relative array of registers `alias` with its own
    // fields.
    (
        @reg $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty)
            => $base:ident + $alias:ident [ $idx:expr ] { $($fields:tt)* }
    ) => {
        ::kernel::static_assert!($idx < <$alias as $crate::io::register::RegisterArray>::SIZE);

        $crate::register!(@bitfield $(#[$attr])* $vis struct $name($storage) { $($fields)* });
        $crate::register!(
            @io_base $name($storage) @
                <$alias as $crate::io::register::Register>::OFFSET +
                $idx * <$alias as $crate::io::register::RegisterArray>::STRIDE
        );
        $crate::register!(@io_relative $vis $name($storage) @ $base);
    };

    // Generates the bitfield for the register.
    //
    // `#[allow(non_camel_case_types)]` is added since register names typically use
    // `SCREAMING_CASE`.
    (
        @bitfield $(#[$attr:meta])* $vis:vis struct $name:ident($storage:ty) { $($fields:tt)* }
    ) => {
        $crate::register!(@bitfield_core
            #[allow(non_camel_case_types)]
            $(#[$attr])* $vis $name $storage
        );
        $crate::register!(@bitfield_fields $vis $name $storage { $($fields)* });
    };

    // `Register` impl shared by all register types: backing storage and base offset.
    (@io_base $name:ident($storage:ty) @ $offset:expr) => {
        impl $crate::io::register::Register for $name {
            type Storage = $storage;

            const OFFSET: usize = $offset;
        }
    };

    // Fixed register: `FixedRegister` marker plus the `$name` location constant.
    (@io_fixed $(#[$attr:meta])* $vis:vis $name:ident ($storage:ty)) => {
        impl $crate::io::register::FixedRegister for $name {}

        $(#[$attr])*
        $vis const $name: $crate::io::register::FixedRegisterLoc<$name> =
            $crate::io::register::FixedRegisterLoc::<$name>::new();
    };

    // Relative register: `WithBase` (base family) plus the `RelativeRegister` marker.
    (@io_relative $vis:vis $name:ident ($storage:ty) @ $base:ident) => {
        impl $crate::io::register::WithBase for $name {
            type BaseFamily = $base;
        }

        impl $crate::io::register::RelativeRegister for $name {}
    };

    // Register array: `Array` builders plus `RegisterArray` (size and stride).
    (@io_array $vis:vis $name:ident ($storage:ty) [ $size:expr, stride = $stride:expr ]) => {
        impl $crate::io::register::Array for $name {}

        impl $crate::io::register::RegisterArray for $name {
            const SIZE: usize = $size;
            const STRIDE: usize = $stride;
        }
    };

    // Relative register array: `WithBase`, `RegisterArray`, and the `RelativeRegisterArray` marker.
    (
        @io_relative_array $vis:vis $name:ident ($storage:ty) [ $size:expr, stride = $stride:expr ]
            @ $base:ident + $offset:literal
    ) => {
        impl $crate::io::register::WithBase for $name {
            type BaseFamily = $base;
        }

        impl $crate::io::register::RegisterArray for $name {
            const SIZE: usize = $size;
            const STRIDE: usize = $stride;
        }

        impl $crate::io::register::RelativeRegisterArray for $name {}
    };

    // Defines the wrapper `$name` type and its conversions from/to the storage type.
    (@bitfield_core $(#[$attr:meta])* $vis:vis $name:ident $storage:ty) => {
        $(#[$attr])*
        #[repr(transparent)]
        #[derive(Clone, Copy, PartialEq, Eq)]
        $vis struct $name {
            inner: $storage,
        }

        #[allow(dead_code)]
        impl $name {
            /// Creates a bitfield from a raw value.
            #[inline(always)]
            $vis const fn from_raw(value: $storage) -> Self {
                Self{ inner: value }
            }

            /// Turns this bitfield into its raw value.
            ///
            /// This is similar to the [`From`] implementation, but is shorter to invoke in
            /// most cases.
            #[inline(always)]
            $vis const fn into_raw(self) -> $storage {
                self.inner
            }
        }

        // SAFETY: `$storage` is `Zeroable` and `$name` is transparent.
        unsafe impl ::pin_init::Zeroable for $name {}

        impl ::core::convert::From<$name> for $storage {
            #[inline(always)]
            fn from(val: $name) -> $storage {
                val.into_raw()
            }
        }

        impl ::core::convert::From<$storage> for $name {
            #[inline(always)]
            fn from(val: $storage) -> $name {
                Self::from_raw(val)
            }
        }
    };

    // Definitions requiring knowledge of individual fields: private and public field accessors,
    // and `Debug` implementation.
    (@bitfield_fields $vis:vis $name:ident $storage:ty {
        $($(#[doc = $doc:expr])* $hi:literal:$lo:literal $field:ident
            $(?=> $try_into_type:ty)?
            $(=> $into_type:ty)?
        ;
        )*
    }
    ) => {
        #[allow(dead_code)]
        impl $name {
        $(
        $crate::register!(@private_field_accessors $vis $name $storage : $hi:$lo $field);
        $crate::register!(
            @public_field_accessors $(#[doc = $doc])* $vis $name $storage : $hi:$lo $field
            $(?=> $try_into_type)?
            $(=> $into_type)?
        );
        )*
        }

        $crate::register!(@debug $name { $($field;)* });
    };

    // Private field accessors working with the exact `Bounded` type for the field.
    (
        @private_field_accessors $vis:vis $name:ident $storage:ty : $hi:tt:$lo:tt $field:ident
    ) => {
        ::kernel::macros::paste!(
        $vis const [<$field:upper _RANGE>]: ::core::ops::RangeInclusive<u8> = $lo..=$hi;
        $vis const [<$field:upper _MASK>]: $storage =
            ((((1 << $hi) - 1) << 1) + 1) - ((1 << $lo) - 1);
        $vis const [<$field:upper _SHIFT>]: u32 = $lo;
        );

        ::kernel::macros::paste!(
        fn [<__ $field>](self) ->
            ::kernel::num::Bounded<$storage, { $hi + 1 - $lo }> {
            // Left shift to align the field's MSB with the storage MSB.
            const ALIGN_TOP: u32 = $storage::BITS - ($hi + 1);
            // Right shift to move the top-aligned field to bit 0 of the storage.
            const ALIGN_BOTTOM: u32 = ALIGN_TOP + $lo;

            // Extract the field using two shifts. `Bounded::shr` produces the correctly-sized
            // output type.
            let val = ::kernel::num::Bounded::<$storage, { $storage::BITS }>::from(
                self.inner << ALIGN_TOP
            );
            val.shr::<ALIGN_BOTTOM, { $hi + 1 - $lo } >()
        }

        const fn [<__with_ $field>](
            mut self,
            value: ::kernel::num::Bounded<$storage, { $hi + 1 - $lo }>,
        ) -> Self
        {
            const MASK: $storage = <$name>::[<$field:upper _MASK>];
            const SHIFT: u32 = <$name>::[<$field:upper _SHIFT>];

            let value = value.get() << SHIFT;
            self.inner = (self.inner & !MASK) | value;

            self
        }
        );
    };

    // Public accessors for fields infallibly (`=>`) converted to a type.
    (
        @public_field_accessors $(#[doc = $doc:expr])* $vis:vis $name:ident $storage:ty :
            $hi:literal:$lo:literal $field:ident => $into_type:ty
    ) => {
        ::kernel::macros::paste!(

        $(#[doc = $doc])*
        #[doc = "Returns the value of this field."]
        #[inline(always)]
        $vis fn $field(self) -> $into_type
        {
            self.[<__ $field>]().into()
        }

        $(#[doc = $doc])*
        #[doc = "Sets this field to the given `value`."]
        #[inline(always)]
        $vis fn [<with_ $field>](self, value: $into_type) -> Self
        {
            self.[<__with_ $field>](value.into())
        }

        );
    };

    // Public accessors for fields fallibly (`?=>`) converted to a type.
    (
        @public_field_accessors $(#[doc = $doc:expr])* $vis:vis $name:ident $storage:ty :
            $hi:tt:$lo:tt $field:ident ?=> $try_into_type:ty
    ) => {
        ::kernel::macros::paste!(

        $(#[doc = $doc])*
        #[doc = "Returns the value of this field."]
        #[inline(always)]
        $vis fn $field(self) ->
            Result<
                $try_into_type,
                <$try_into_type as ::core::convert::TryFrom<
                    ::kernel::num::Bounded<$storage, { $hi + 1 - $lo }>
                >>::Error
            >
        {
            self.[<__ $field>]().try_into()
        }

        $(#[doc = $doc])*
        #[doc = "Sets this field to the given `value`."]
        #[inline(always)]
        $vis fn [<with_ $field>](self, value: $try_into_type) -> Self
        {
            self.[<__with_ $field>](value.into())
        }

        );
    };

    // Public accessors for fields not converted to a type.
    (
        @public_field_accessors $(#[doc = $doc:expr])* $vis:vis $name:ident $storage:ty :
            $hi:tt:$lo:tt $field:ident
    ) => {
        ::kernel::macros::paste!(

        $(#[doc = $doc])*
        #[doc = "Returns the value of this field."]
        #[inline(always)]
        $vis fn $field(self) ->
            ::kernel::num::Bounded<$storage, { $hi + 1 - $lo }>
        {
            self.[<__ $field>]()
        }

        $(#[doc = $doc])*
        #[doc = "Sets this field to the compile-time constant `VALUE`."]
        #[inline(always)]
        $vis const fn [<with_const_ $field>]<const VALUE: $storage>(self) -> Self {
            self.[<__with_ $field>](
                ::kernel::num::Bounded::<$storage, { $hi + 1 - $lo }>::new::<VALUE>()
            )
        }

        $(#[doc = $doc])*
        #[doc = "Sets this field to the given `value`."]
        #[inline(always)]
        $vis fn [<with_ $field>]<T>(
            self,
            value: T,
        ) -> Self
            where T: Into<::kernel::num::Bounded<$storage, { $hi + 1 - $lo }>>,
        {
            self.[<__with_ $field>](value.into())
        }

        $(#[doc = $doc])*
        #[doc = "Tries to set this field to `value`, returning an error if it is out of range."]
        #[inline(always)]
        $vis fn [<try_with_ $field>]<T>(
            self,
            value: T,
        ) -> ::kernel::error::Result<Self>
            where T: ::kernel::num::TryIntoBounded<$storage, { $hi + 1 - $lo }>,
        {
            Ok(
                self.[<__with_ $field>](
                    value.try_into_bounded().ok_or(::kernel::error::code::EOVERFLOW)?
                )
            )
        }

        );
    };

    // `Debug` implementation.
    (@debug $name:ident { $($field:ident;)* }) => {
        impl ::kernel::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::kernel::fmt::Formatter<'_>) -> ::kernel::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("<raw>", &::kernel::prelude::fmt!("{:#x}", self.inner))
                $(
                    .field(stringify!($field), &self.$field())
                )*
                    .finish()
            }
        }
    };
}
