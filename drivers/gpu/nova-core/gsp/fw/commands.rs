// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use core::ops::Range;

use kernel::{
    bitfield,
    device,
    pci,
    prelude::*,
    transmute::{
        AsBytes,
        FromBytes, //
    }, //
};

use crate::{
    gpu::Chipset,
    gsp::GSP_PAGE_SIZE,
    num::IntoSafeCast, //
};

use crate::gsp::nvkv::{
    nvkv_decode,
    nvkv_encode,
    Accumulated,
    Array,
    ArrayVec,
    DecoderValue,
    Encodeable,
    Encoder,
    Index,
    Key,
    KeyId,
    Required, //
};

use super::bindings;

/// Payload of the `GspSetSystemInfo` command.
#[repr(transparent)]
pub(crate) struct GspSetSystemInfo {
    inner: bindings::GspSystemInfo,
}
static_assert!(size_of::<GspSetSystemInfo>() < GSP_PAGE_SIZE);

impl GspSetSystemInfo {
    /// Returns an in-place initializer for the `GspSetSystemInfo` command.
    pub(crate) fn init<'a>(
        dev: &'a pci::Device<device::Bound>,
        chipset: Chipset,
    ) -> impl Init<Self, Error> + 'a {
        type InnerGspSystemInfo = bindings::GspSystemInfo;
        let pci_config_mirror_range = chipset.pci_config_mirror_range();
        let init_inner = try_init!(InnerGspSystemInfo {
            gpuPhysAddr: dev.resource_start(0)?,
            gpuPhysFbAddr: dev.resource_start(1)?,
            gpuPhysInstAddr: dev.resource_start(3)?,
            nvDomainBusDeviceFunc: u64::from(dev.dev_id()),

            // Using TASK_SIZE in r535_gsp_rpc_set_system_info() seems wrong because
            // TASK_SIZE is per-task. That's probably a design issue in GSP-RM though.
            maxUserVa: (1 << 47) - 4096,
            pciConfigMirrorBase: pci_config_mirror_range.start,
            pciConfigMirrorSize: pci_config_mirror_range.end - pci_config_mirror_range.start,

            PCIDeviceID: (u32::from(dev.device_id()) << 16) | u32::from(dev.vendor_id().as_raw()),
            PCISubDeviceID: (u32::from(dev.subsystem_device_id()) << 16)
                | u32::from(dev.subsystem_vendor_id()),
            PCIRevisionID: u32::from(dev.revision_id()),
            bIsPrimary: 0,
            bPreserveVideoMemoryAllocations: 0,
            ..Zeroable::init_zeroed()
        });

        try_init!(GspSetSystemInfo {
            inner <- init_inner,
        })
    }
}

// SAFETY: These structs don't meet the no-padding requirements of AsBytes but
//         that is not a problem because they are not used outside the kernel.
unsafe impl AsBytes for GspSetSystemInfo {}

// SAFETY: These structs don't meet the no-padding requirements of FromBytes but
//         that is not a problem because they are not used outside the kernel.
unsafe impl FromBytes for GspSetSystemInfo {}

#[repr(transparent)]
pub(crate) struct PackedRegistryEntry(bindings::PACKED_REGISTRY_ENTRY);

impl PackedRegistryEntry {
    pub(crate) fn new(offset: u32, value: u32) -> Self {
        Self({
            bindings::PACKED_REGISTRY_ENTRY {
                nameOffset: offset,

                // We only support DWORD types for now. Support for other types
                // will come later if required.
                type_: bindings::REGISTRY_TABLE_ENTRY_TYPE_DWORD as u8,
                __bindgen_padding_0: Default::default(),
                data: value,
                length: 0,
            }
        })
    }
}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for PackedRegistryEntry {}

/// Payload of the `SetRegistry` command.
#[repr(transparent)]
pub(crate) struct PackedRegistryTable {
    inner: bindings::PACKED_REGISTRY_TABLE,
}

impl PackedRegistryTable {
    pub(crate) fn init(num_entries: u32, size: u32) -> impl Init<Self> {
        type InnerPackedRegistryTable = bindings::PACKED_REGISTRY_TABLE;
        let init_inner = init!(InnerPackedRegistryTable {
            numEntries: num_entries,
            size,
            entries: Default::default()
        });

        init!(PackedRegistryTable { inner <- init_inner })
    }
}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for PackedRegistryTable {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for PackedRegistryTable {}

/// Payload of the `GetGspStaticInfo` command and message.
#[repr(transparent)]
#[derive(Zeroable)]
pub(crate) struct GspStaticConfigInfo(bindings::GspStaticConfigInfo_t);

impl GspStaticConfigInfo {
    /// Returns a bytes array containing the (hopefully) zero-terminated name of this GPU.
    pub(crate) fn gpu_name_str(&self) -> [u8; 64] {
        self.0.gpuNameString
    }

    /// Returns an iterator over valid FB regions from GSP firmware data.
    fn fb_regions(
        &self,
    ) -> impl Iterator<Item = &bindings::NV2080_CTRL_CMD_FB_GET_FB_REGION_FB_REGION_INFO> {
        let fb_info = &self.0.fbRegionInfoParams;
        fb_info
            .fbRegion
            .iter()
            .take(fb_info.numFBRegions.into_safe_cast())
            .filter(|reg| reg.limit >= reg.base)
    }

    /// Iterates over usable FB regions from GSP firmware data.
    ///
    /// Each yielded region is a [`Range<u64>`] suitable for driver memory allocation.
    /// Usable regions are those that satisfy all the following properties:
    /// - Are not reserved for firmware internal use.
    /// - Are not protected (hardware-enforced access restrictions).
    /// - Support compression (can use GPU memory compression for bandwidth).
    /// - Support ISO (isochronous memory for display requiring guaranteed bandwidth).
    pub(crate) fn usable_fb_regions(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.fb_regions().filter_map(|reg| {
            // Filter: not reserved, not protected, supports compression and ISO.
            if reg.reserved == 0
                && reg.bProtected == 0
                && reg.supportCompressed != 0
                && reg.supportISO != 0
            {
                reg.limit.checked_add(1).map(|end| reg.base..end)
            } else {
                None
            }
        })
    }
}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for GspStaticConfigInfo {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for GspStaticConfigInfo {}

/// Power level requested to the [`UnloadingGuestDriver`] command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
#[expect(unused)]
pub(crate) enum PowerStateLevel {
    /// Full unload.
    Level0 = bindings::NV2080_CTRL_GPU_SET_POWER_STATE_GPU_LEVEL_0,
    /// S3 (suspend to RAM).
    Level3 = bindings::NV2080_CTRL_GPU_SET_POWER_STATE_GPU_LEVEL_3,
    /// Hibernate (suspend to disk).
    Level7 = bindings::NV2080_CTRL_GPU_SET_POWER_STATE_GPU_LEVEL_7,
}

impl PowerStateLevel {
    /// Returns `true` if this state represents a power management transition, i.e. some GPU state
    /// must survive it (as opposed to a full unload).
    pub(crate) fn is_power_transition(self) -> bool {
        self != PowerStateLevel::Level0
    }
}

/// Payload of the `UnloadingGuestDriver` command and message.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Zeroable)]
pub(crate) struct UnloadingGuestDriver(bindings::rpc_unloading_guest_driver_v1F_07);

impl UnloadingGuestDriver {
    pub(crate) fn new(level: PowerStateLevel) -> Self {
        Self(bindings::rpc_unloading_guest_driver_v1F_07 {
            bInPMTransition: u8::from(level.is_power_transition()),
            bGc6Entering: 0,
            newLevel: level as u32,
            ..Zeroable::zeroed()
        })
    }
}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for UnloadingGuestDriver {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for UnloadingGuestDriver {}

/// The host CPU architecture.
#[derive(Clone, Copy)]
pub(crate) enum OorArch {
    None = 0,
    X86_64 = 1,
    Ppc64le = 2,
    Arm = 3,
    Aarch64 = 4,
    Riscv64 = 5,
}

impl OorArch {
    /// Returns the variant naming the architecture this kernel is built for.
    fn host() -> Self {
        if cfg!(target_arch = "x86_64") {
            Self::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else if cfg!(target_arch = "powerpc64") {
            Self::Ppc64le
        } else if cfg!(target_arch = "arm") {
            Self::Arm
        } else if cfg!(target_arch = "riscv64") {
            Self::Riscv64
        } else {
            Self::None
        }
    }
}

// TODO[FPRI]: This is a temporary solution to be replaced with the corresponding derive macros once
// they land.
impl TryFrom<u32> for OorArch {
    type Error = Error;

    fn try_from(value: u32) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::X86_64),
            2 => Ok(Self::Ppc64le),
            3 => Ok(Self::Arm),
            4 => Ok(Self::Aarch64),
            5 => Ok(Self::Riscv64),
            _ => Err(EINVAL),
        }
    }
}

impl From<OorArch> for u32 {
    fn from(value: OorArch) -> Self {
        value as u32
    }
}

nvkv_encode! {
    /// A GSP registry entry.
    pub(crate) struct RegKey {
        key_name: Key<&'static [u8], { Self::REGKEY_NAME_KEY }>,
        key_value: Key<u32, { Self::REGKEY_VALUE_U32_KEY }>,
    }
}

impl RegKey {
    const REGKEY_NAME_KEY: KeyId = 0x3070;
    const REGKEY_VALUE_U32_KEY: KeyId = 0x3071;

    /// Creates a registry entry. `key_name` must include its NULL terminator, which GSP-RM counts
    /// in the encoded name length.
    pub(crate) fn new(key_name: &'static [u8], key_value: u32) -> Self {
        Self {
            key_name: key_name.into(),
            key_value: key_value.into(),
        }
    }
}

impl Encodeable for KVVec<RegKey> {
    fn encode(&self, encoder: &mut Encoder) -> Result {
        for regkey in self {
            regkey.encode(encoder)?;
        }
        Ok(())
    }
}

nvkv_encode! {
    /// SR-IOV virtual function information.
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    struct VfInfo {
        total_vfs: Key<u32, { Self::VF_TOTAL_VFS_KEY }>,
        first_vf_offset: Key<u32, { Self::VF_FIRST_VF_OFFSET_KEY }>,
        flags: Key<u64, { Self::VF_FLAGS_KEY }>,
        first_bar0_address: Key<u64, { Self::VF_FIRST_BAR0_ADDRESS_KEY }>,
        first_bar1_address: Key<u64, { Self::VF_FIRST_BAR1_ADDRESS_KEY }>,
        first_bar2_address: Key<u64, { Self::VF_FIRST_BAR2_ADDRESS_KEY }>,
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl VfInfo {
    const VF_TOTAL_VFS_KEY: KeyId = 0x0080;
    const VF_FIRST_VF_OFFSET_KEY: KeyId = 0x0081;
    const VF_FLAGS_KEY: KeyId = 0x1003;
    const VF_FIRST_BAR0_ADDRESS_KEY: KeyId = 0x1050;
    const VF_FIRST_BAR1_ADDRESS_KEY: KeyId = 0x1051;
    const VF_FIRST_BAR2_ADDRESS_KEY: KeyId = 0x1052;
}

nvkv_encode! {
    /// Payload of the `GSP_INIT` command.
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    pub(crate) struct GspInitRequest {
        pci_device_id: Key<u32, { Self::PCI_DEVICE_ID_KEY }>,
        pci_sub_device_id: Key<u32, { Self::PCI_SUBDEVICE_ID_KEY }>,
        pci_revision_id: Key<u32, { Self::PCI_REVISION_ID_KEY }>,
        pci_config_mirror_base: Key<u32, { Self::PCI_CONFIG_MIRROR_BASE_KEY }>,
        pci_config_mirror_size: Key<u32, { Self::PCI_CONFIG_MIRROR_SIZE_KEY }>,
        oor_arch: Key<OorArch, { Self::OOR_ARCH_KEY }, u32>,
        bus_device_func: Key<u64, { Self::NV_DOMAIN_BUS_DEVICE_FUNC_KEY }>,
        regkeys: KVVec<RegKey>,
        vf_info: Option<VfInfo>,
    }
}

bitfield! {
    /// PCI bus, device and function, packed the way `PCI_DEVID` packs them, which is what
    /// [`pci::Device::dev_id`] returns.
    struct PciDevId(u16) {
        15:8 bus;
        7:3 device;
        2:0 function;
    }
}

bitfield! {
    /// A GPU's PCI location, encoded the way GSP-RM decodes it, matching Open RM's
    /// `gpuEncodeDomainBusDevice`. Despite the name GSP-RM gives the key, the function number
    /// is not part of it.
    struct DomainBusDevice(u64) {
        63:32 domain;
        15:8 bus;
        7:0 device;
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl GspInitRequest {
    const PCI_DEVICE_ID_KEY: KeyId = 0x0001;
    const PCI_SUBDEVICE_ID_KEY: KeyId = 0x0002;
    const PCI_REVISION_ID_KEY: KeyId = 0x0003;
    const PCI_CONFIG_MIRROR_BASE_KEY: KeyId = 0x0010;
    const PCI_CONFIG_MIRROR_SIZE_KEY: KeyId = 0x0011;
    const OOR_ARCH_KEY: KeyId = 0x0070;
    const NV_DOMAIN_BUS_DEVICE_FUNC_KEY: KeyId = 0x1020;

    /// Describes `dev` to GSP-RM and asks it to apply `regkeys`.
    pub(crate) fn new(
        dev: &pci::Device<device::Bound>,
        chipset: Chipset,
        regkeys: KVVec<RegKey>,
    ) -> Self {
        let mirror = chipset.pci_config_mirror_range();
        let dev_id = PciDevId::from(dev.dev_id());
        let bus_device_func = DomainBusDevice::zeroed()
            .with_domain(dev.domain_nr())
            .with_bus(u8::from(dev_id.bus()))
            .with_device(u8::from(dev_id.device()));
        let device_id = (u32::from(dev.device_id()) << 16) | u32::from(dev.vendor_id().as_raw());
        let sub_device_id =
            (u32::from(dev.subsystem_device_id()) << 16) | u32::from(dev.subsystem_vendor_id());

        Self {
            pci_device_id: device_id.into(),
            pci_sub_device_id: sub_device_id.into(),
            pci_revision_id: u32::from(dev.revision_id()).into(),
            pci_config_mirror_base: mirror.start.into(),
            pci_config_mirror_size: (mirror.end - mirror.start).into(),
            oor_arch: OorArch::host().into(),
            bus_device_func: u64::from(bus_device_func).into(),
            regkeys,
            vf_info: None,
        }
    }
}

// Decode:

// Should decode with UnknownKeyPolicy::Ignore.
nvkv_decode! {
    /// Schema for the `GSP_INIT` response.
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    #[derive(Default)]
    struct GspInitResponseSchema => GspInitResponse {
        gpu_name:
            Array<u8, { GspInitResponse::MAX_GPU_NAME_LEN }, { Self::GPU_NAME_STRING_KEY }>,
        fb_regions: Accumulated<FbRegionSchema>,
        bar1_pde_base: Required<u64, { Self::BAR1_PDE_BASE_KEY }>,
        vmmu_segment_size: Key<u64, { Self::VMMU_SEGMENT_SIZE_KEY }>,
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl GspInitResponseSchema {
    const GPU_NAME_STRING_KEY: KeyId = 0x2000;
    const BAR1_PDE_BASE_KEY: KeyId = 0x1020;
    const VMMU_SEGMENT_SIZE_KEY: KeyId = 0x1050;
}

/// Payload of the `GSP_INIT` response.
#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
struct GspInitResponse {
    gpu_name: ArrayVec<u8, { Self::MAX_GPU_NAME_LEN }>,
    fb_regions: KVVec<FbRegion>,
    bar1_pde_base: u64,
    vmmu_segment_size: u64,
}

impl GspInitResponse {
    const MAX_GPU_NAME_LEN: usize = 64;
}

nvkv_decode! {
    /// Schema for one FB region of the `GSP_INIT` response.
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    #[derive(Default)]
    struct FbRegionSchema => FbRegion {
        base: Required<u64, { Self::BASE_KEY }>,
        limit: Required<u64, { Self::LIMIT_KEY }>,
        flags: Required<FbRegionFlags, { Self::FLAGS_KEY }>,
        tag: Required<u32, { Self::TAG_KEY }>,
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl FbRegionSchema {
    const BASE_KEY: KeyId = 0x1011;
    const LIMIT_KEY: KeyId = 0x1012;
    const FLAGS_KEY: KeyId = 0x0012;
    const TAG_KEY: KeyId = 0x0013;
}

bitfield! {
    /// FB region attribute flags.
    struct FbRegionFlags(u32) {
        0:0 support_compressed => bool;
        1:1 support_iso => bool;
        2:2 protected => bool;
    }
}

impl TryFrom<DecoderValue<'_>> for FbRegionFlags {
    type Error = Error;

    fn try_from(value: DecoderValue<'_>) -> Result<Self> {
        if let DecoderValue::Scalar32(v) = value {
            Ok(v.into())
        } else {
            Err(EINVAL)
        }
    }
}

/// One FB memory region.
#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
struct FbRegion {
    base: u64,
    limit: u64,
    flags: FbRegionFlags,
    tag: u32,
}

// vGPU related:

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum HypervisorType {
    Xen = 0,
    Vmware = 1,
    HyperV = 2,
    Kvm = 3,
    Unknown = 4,
}

impl From<HypervisorType> for u32 {
    fn from(value: HypervisorType) -> Self {
        value as u32
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum CpuArch {
    X86_64 = 2,
}

impl From<CpuArch> for u32 {
    fn from(value: CpuArch) -> Self {
        value as u32
    }
}

bitfield! {
    pub(crate) struct Dbdf(u32) {
        2:0 function;
        7:3 device;
        15:8 bus;
        31:16 domain;
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SwizzId(pub(crate) u32);

impl SwizzId {
    pub(crate) const WHOLE_GPU: Self = Self(0xFFFF_FFFF);
}

impl Default for SwizzId {
    fn default() -> Self {
        Self::WHOLE_GPU
    }
}

impl From<SwizzId> for u32 {
    fn from(value: SwizzId) -> Self {
        value.0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MigrationFeature(pub(crate) u32);

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl MigrationFeature {
    const KVM: Self = Self(0x4000);
}

impl From<MigrationFeature> for u32 {
    fn from(value: MigrationFeature) -> Self {
        value.0
    }
}

bitfield! {
    pub(crate) struct FeatureFlags(u64) {
        3:3 enable_uvm => bool;
        5:5 vmm_migration => bool;
    }
}

bitfield! {
    pub(crate) struct ChannelMapEntry(u64) {
        15:0 engine_type;
        31:16 index;
        63:32 chid_offset;
    }
}

impl ChannelMapEntry {
    const KEY: KeyId = 0x1001;
}

impl Encodeable for KVVec<ChannelMapEntry> {
    fn encode(&self, encoder: &mut Encoder) -> Result {
        // SAFETY: `ChannelMapEntry` is a `bitfield!` over `u64`, i.e.
        // `#[repr(transparent)]` around a `u64`, so the entries are
        // layout-compatible with `u64` and can be viewed as a `u64` slice.
        let slice = unsafe { core::slice::from_raw_parts(self.as_ptr().cast::<u64>(), self.len()) };
        encoder.encode_array64(ChannelMapEntry::KEY, Index::new::<0>(), slice)
    }
}

bitfield! {
    pub(crate) struct VgpuBootloadOptions(u64) {
    }
}

// VGPU_BOOTLOAD

nvkv_encode! {
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    struct VgpuBootloadRequest {
        dbdf: Key<Dbdf, { Self::DBDF_KEY }, u32>,
        gfid: Key<u32, { Self::GFID_KEY }>,
        vgpu_type: Key<u32, { Self::VGPU_TYPE_KEY }>,
        vm_pid: Key<u32, { Self::VM_PID_KEY }>,
        swizz_id: Key<SwizzId, { Self::SWIZZ_ID_KEY }, u32>,
        num_channels: Key<u32, { Self::NUM_CHANNELS_KEY }>,
        num_plugin_channels: Key<u32, { Self::NUM_PLUGIN_CHANNELS_KEY }>,
        guest_fb_segment_count: Key<u32, { Self::GUEST_FB_SEGMENT_COUNT_KEY }>,
        options: Key<VgpuBootloadOptions, { Self::OPTIONS_KEY }, u64>,
        channel_mapping: KVVec<ChannelMapEntry>,
        guest_fb_segment_phys_addr: Array<u64, 8, { Self::GUEST_FB_SEGMENT_PHYS_ADDR_KEY }>,
        guest_fb_segment_length: Array<u64, 8, { Self::GUEST_FB_SEGMENT_LENGTH_KEY }>,
        plugin_heap_phys_addr: Key<u64, { Self::PLUGIN_HEAP_PHYS_ADDR_KEY }>,
        plugin_heap_length: Key<u64, { Self::PLUGIN_HEAP_LENGTH_KEY }>,
        ctrl_buff_offset: Key<u64, { Self::CTRL_BUFF_OFFSET_KEY }>,
        init_task_log_offset: Key<u64, { Self::INIT_TASK_LOG_OFFSET_KEY }>,
        init_task_log_size: Key<u64, { Self::INIT_TASK_LOG_SIZE_KEY }>,
        vgpu_task_log_offset: Key<u64, { Self::VGPU_TASK_LOG_OFFSET_KEY }>,
        vgpu_task_log_size: Key<u64, { Self::VGPU_TASK_LOG_SIZE_KEY }>,
        kernel_log_offset: Key<u64, { Self::KERNEL_LOG_OFFSET_KEY }>,
        kernel_log_size: Key<u64, { Self::KERNEL_LOG_SIZE_KEY }>,
        mig_rm_heap_phys_addr: Key<u64, { Self::MIG_RM_HEAP_PHYS_ADDR_KEY }>,
        mig_rm_heap_length: Key<u64, { Self::MIG_RM_HEAP_LENGTH_KEY }>,
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl VgpuBootloadRequest {
    const DBDF_KEY: KeyId = 0x0001;
    const GFID_KEY: KeyId = 0x0002;
    const VGPU_TYPE_KEY: KeyId = 0x0003;
    const VM_PID_KEY: KeyId = 0x0004;
    const SWIZZ_ID_KEY: KeyId = 0x0005;
    const NUM_CHANNELS_KEY: KeyId = 0x0006;
    const NUM_PLUGIN_CHANNELS_KEY: KeyId = 0x0007;
    const GUEST_FB_SEGMENT_COUNT_KEY: KeyId = 0x0008;
    const OPTIONS_KEY: KeyId = 0x1000;
    const GUEST_FB_SEGMENT_PHYS_ADDR_KEY: KeyId = 0x1002;
    const GUEST_FB_SEGMENT_LENGTH_KEY: KeyId = 0x1003;
    const PLUGIN_HEAP_PHYS_ADDR_KEY: KeyId = 0x1004;
    const PLUGIN_HEAP_LENGTH_KEY: KeyId = 0x1005;
    const CTRL_BUFF_OFFSET_KEY: KeyId = 0x1006;
    const INIT_TASK_LOG_OFFSET_KEY: KeyId = 0x1007;
    const INIT_TASK_LOG_SIZE_KEY: KeyId = 0x1008;
    const VGPU_TASK_LOG_OFFSET_KEY: KeyId = 0x1009;
    const VGPU_TASK_LOG_SIZE_KEY: KeyId = 0x100A;
    const KERNEL_LOG_OFFSET_KEY: KeyId = 0x100B;
    const KERNEL_LOG_SIZE_KEY: KeyId = 0x100C;
    const MIG_RM_HEAP_PHYS_ADDR_KEY: KeyId = 0x100D;
    const MIG_RM_HEAP_LENGTH_KEY: KeyId = 0x100E;
}

// VGPU_MGMT_QUERY_PROPERTIES

nvkv_decode! {
    #[derive(Default)]
    struct VgpuPropertiesSchema => VgpuProperties {
        // TODO: `name`/`class` required?
        name: Array<u8, { VgpuProperties::STRING_LEN }, { Self::TYPE_NAME_KEY }>,
        class: Array<u8, { VgpuProperties::STRING_LEN }, { Self::CLASS_KEY }>,
        type_id: Required<u32, { Self::TYPE_ID_KEY }>,
        bar1_length: Required<u64, { Self::BAR1_LENGTH_KEY }>,
        max_instance: Required<u32, { Self::MAX_INSTANCE_KEY }>,
        ecc: Key<u32, { Self::ECC_KEY }>,
        profile_size: Required<u64, { Self::PROFILE_SIZE_KEY }>,
        max_fps: Key<u32, { Self::MAX_FPS_KEY }>,
        num_heads: Key<u32, { Self::NUM_HEADS_KEY }>,
        max_res_x: Key<u32, { Self::MAX_RES_X_KEY }>,
        max_res_y: Key<u32, { Self::MAX_RES_Y_KEY }>,
        dev_id: Required<u32, { Self::DEV_ID_KEY }>,
        subsystem_id: Required<u32, { Self::SUBSYSTEM_ID_KEY }>,
        fb_length: Required<u64, { Self::FB_LENGTH_KEY }>,
        gsp_heap_size: Required<u64, { Self::GSP_HEAP_SIZE_KEY }>,
        fb_reservation: Required<u64, { Self::FB_RESERVATION_KEY }>,
    }
}

impl VgpuPropertiesSchema {
    const TYPE_NAME_KEY: KeyId = 0x3100;
    const CLASS_KEY: KeyId = 0x3101;
    const TYPE_ID_KEY: KeyId = 0x3102;
    const BAR1_LENGTH_KEY: KeyId = 0x3103;
    const MAX_INSTANCE_KEY: KeyId = 0x3104;
    const ECC_KEY: KeyId = 0x3105;
    const PROFILE_SIZE_KEY: KeyId = 0x3106;
    const MAX_FPS_KEY: KeyId = 0x3107;
    const NUM_HEADS_KEY: KeyId = 0x3108;
    const MAX_RES_X_KEY: KeyId = 0x3109;
    const MAX_RES_Y_KEY: KeyId = 0x310A;
    const DEV_ID_KEY: KeyId = 0x310B;
    const SUBSYSTEM_ID_KEY: KeyId = 0x310C;
    const FB_LENGTH_KEY: KeyId = 0x310D;
    const GSP_HEAP_SIZE_KEY: KeyId = 0x310E;
    const FB_RESERVATION_KEY: KeyId = 0x310F;
}

struct VgpuProperties {
    name: ArrayVec<u8, { Self::STRING_LEN }>,
    class: ArrayVec<u8, { Self::STRING_LEN }>,
    type_id: u32,
    bar1_length: u64,
    max_instance: u32,
    ecc: u32,
    profile_size: u64,
    max_fps: u32,
    num_heads: u32,
    max_res_x: u32,
    max_res_y: u32,
    dev_id: u32,
    subsystem_id: u32,
    fb_length: u64,
    gsp_heap_size: u64,
    fb_reservation: u64,
}

impl VgpuProperties {
    const STRING_LEN: usize = 64;
}

// SETUP_CONFIG_PARAMS_AND_INIT

nvkv_encode! {
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    struct PluginConfigParamsRequest {
        uuid: Key<[u8; 16], { Self::UUID_KEY }>,
        dbdf: Key<Dbdf, { Self::DBDF_KEY }, u32>,
        dev_inst: Key<u32, { Self::DEV_INST_KEY }>,
        vgpu_type: Key<u32, { Self::VGPU_TYPE_KEY }>,
        vm_pid: Key<u32, { Self::VM_PID_KEY }>,
        swizz_id: Key<SwizzId, { Self::SWIZZ_ID_KEY }, u32>,
        num_channels: Key<u32, { Self::NUM_CHANNELS_KEY }>,
        num_plugin_channels: Key<u32, { Self::NUM_PLUGIN_CHANNELS_KEY }>,
        vmm_cap: Key<u32, { Self::VMM_CAP_KEY }>,
        migration_feature: Key<MigrationFeature, { Self::MIGRATION_FEATURE_KEY }, u32>,
        hypervisor_type: Key<HypervisorType, { Self::HYPERVISOR_TYPE_KEY }, u32>,
        cpu_arch: Key<CpuArch, { Self::CPU_ARCH_KEY }, u32>,
        page_size: Key<u64, { Self::PAGE_SIZE_KEY }>,
        feature_flags: Key<FeatureFlags, { Self::FEATURE_FLAGS_KEY }, u64>,
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl PluginConfigParamsRequest {
    const UUID_KEY: KeyId = 0x0001;
    const DBDF_KEY: KeyId = 0x0002;
    const DEV_INST_KEY: KeyId = 0x0004;
    const VGPU_TYPE_KEY: KeyId = 0x0005;
    const VM_PID_KEY: KeyId = 0x0006;
    const SWIZZ_ID_KEY: KeyId = 0x0010;
    const NUM_CHANNELS_KEY: KeyId = 0x0011;
    const NUM_PLUGIN_CHANNELS_KEY: KeyId = 0x0012;
    const VMM_CAP_KEY: KeyId = 0x0020;
    const MIGRATION_FEATURE_KEY: KeyId = 0x0021;
    const HYPERVISOR_TYPE_KEY: KeyId = 0x0022;
    const CPU_ARCH_KEY: KeyId = 0x0023;
    const PAGE_SIZE_KEY: KeyId = 0x0024;
    const FEATURE_FLAGS_KEY: KeyId = 0x0030;
}

// UPDATE_BME_STATE

nvkv_encode! {
    #[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
    struct PluginSetBmeRequest {
        bme_enable: Key<bool, { Self::BME_ENABLE_KEY }, u32>,
    }
}

#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
impl PluginSetBmeRequest {
    const BME_ENABLE_KEY: KeyId = 0x0100;
}

#[kunit_tests(nova_core_fw_commands)]
mod tests {
    use crate::gsp::nvkv::{
        Decoder,
        Index,
        UnknownKeyPolicy, //
    };

    use super::*;

    #[test]
    fn gsp_init_request() -> Result {
        let mut encoder = Encoder::new();

        let mut regkeys = KVVec::new();
        regkeys.push(
            RegKey {
                key_name: b"test_key\0".into(),
                key_value: 0xdead_beef.into(),
            },
            GFP_KERNEL,
        )?;

        let gsp_init = GspInitRequest {
            pci_device_id: 45.into(),
            pci_sub_device_id: 67.into(),
            pci_revision_id: 3.into(),
            pci_config_mirror_base: 0x1234_5678.into(),
            pci_config_mirror_size: 0x1000.into(),
            oor_arch: OorArch::Aarch64.into(),
            bus_device_func: 0x0001_0203_0405_0607.into(),
            regkeys,
            vf_info: Some(VfInfo {
                total_vfs: 8.into(),
                first_vf_offset: 1.into(),
                flags: 0x7.into(),
                first_bar0_address: 0x1000_0000.into(),
                first_bar1_address: 0x2000_0000.into(),
                first_bar2_address: 0x3000_0000.into(),
            }),
        };

        gsp_init.encode(&mut encoder)?;
        let _encoded = encoder.finish();
        Ok(())
    }

    #[test]
    fn encode_vgpu_bootload_request() -> Result {
        let mut encoder = Encoder::new();

        let mut channel_mapping = KVVec::new();
        channel_mapping.push(
            ChannelMapEntry::zeroed().with_const_engine_type::<1>(),
            GFP_KERNEL,
        )?;
        channel_mapping.push(
            ChannelMapEntry::zeroed()
                .with_const_engine_type::<2>()
                .with_const_chid_offset::<8>(),
            GFP_KERNEL,
        )?;

        let request = VgpuBootloadRequest {
            dbdf: Dbdf::zeroed()
                .with_const_domain::<1>()
                .with_const_bus::<2>()
                .with_const_device::<0>()
                .with_const_function::<3>()
                .into(),
            gfid: 1.into(),
            vgpu_type: 0x42.into(),
            vm_pid: 1234.into(),
            swizz_id: SwizzId(3).into(),
            num_channels: 8.into(),
            num_plugin_channels: 2.into(),
            guest_fb_segment_count: 1.into(),
            options: VgpuBootloadOptions::zeroed().into(),
            channel_mapping,
            guest_fb_segment_phys_addr: Array::new(&[0x1_0000_0000])?,
            guest_fb_segment_length: Array::new(&[0x4000_0000])?,
            plugin_heap_phys_addr: 0x2000_0000.into(),
            plugin_heap_length: 0x10_0000.into(),
            ctrl_buff_offset: 0.into(),
            init_task_log_offset: 0x100.into(),
            init_task_log_size: 0x200.into(),
            vgpu_task_log_offset: 0x300.into(),
            vgpu_task_log_size: 0x400.into(),
            kernel_log_offset: 0x500.into(),
            kernel_log_size: 0x600.into(),
            mig_rm_heap_phys_addr: 0x3000_0000.into(),
            mig_rm_heap_length: 0x40_0000.into(),
        };

        request.encode(&mut encoder)?;
        let _encoded = encoder.finish();
        Ok(())
    }

    #[test]
    fn encode_plugin_config_params() -> Result {
        let mut encoder = Encoder::new();

        let request = PluginConfigParamsRequest {
            uuid: [
                0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88,
            ]
            .into(),
            dbdf: Dbdf::zeroed()
                .with_const_domain::<1>()
                .with_const_bus::<2>()
                .with_const_device::<0>()
                .with_const_function::<3>()
                .into(),
            dev_inst: 0.into(),
            vgpu_type: 0x42.into(),
            vm_pid: 1234.into(),
            swizz_id: SwizzId(3).into(),
            num_channels: 8.into(),
            num_plugin_channels: 2.into(),
            vmm_cap: 0.into(),
            migration_feature: MigrationFeature::KVM.into(),
            hypervisor_type: HypervisorType::Unknown.into(),
            cpu_arch: CpuArch::X86_64.into(),
            page_size: 4096.into(),
            feature_flags: FeatureFlags::zeroed()
                .with_enable_uvm(true)
                .with_vmm_migration(true)
                .into(),
        };

        request.encode(&mut encoder)?;
        let _encoded = encoder.finish();
        Ok(())
    }

    #[test]
    fn encode_plugin_set_bme() -> Result {
        let mut encoder = Encoder::new();

        let request = PluginSetBmeRequest {
            bme_enable: true.into(),
        };

        request.encode(&mut encoder)?;
        let _encoded = encoder.finish();
        Ok(())
    }

    #[test]
    fn decode_vgpu_properties() -> Result {
        let name = b"test name\0";
        let class = b"test class\0";
        const TYPE_ID: u32 = 0x42;
        const BAR1_LENGTH: u64 = 0x1_0000_0000;
        const MAX_INSTANCE: u32 = 4;
        const ECC: u32 = 1;
        const PROFILE_SIZE: u64 = 0x1_0000_0000;
        const MAX_FPS: u32 = 60;
        const NUM_HEADS: u32 = 4;
        const MAX_RES_X: u32 = 7680;
        const MAX_RES_Y: u32 = 4320;
        const DEV_ID: u32 = 0x1db4;
        const SUBSYSTEM_ID: u32 = 0x1234;
        const FB_LENGTH: u64 = 0x1_0000_0000;
        const GSP_HEAP_SIZE: u64 = 0x10_0000;
        const FB_RESERVATION: u64 = 0x40_0000;

        let index = Index::new::<0>();
        let mut encoder = Encoder::new();
        encoder.encode_array8(VgpuPropertiesSchema::TYPE_NAME_KEY, index, name)?;
        encoder.encode_array8(VgpuPropertiesSchema::CLASS_KEY, index, class)?;
        encoder.encode_u32(VgpuPropertiesSchema::TYPE_ID_KEY, index, TYPE_ID)?;
        encoder.encode_u64(VgpuPropertiesSchema::BAR1_LENGTH_KEY, index, BAR1_LENGTH)?;
        encoder.encode_u32(VgpuPropertiesSchema::MAX_INSTANCE_KEY, index, MAX_INSTANCE)?;
        encoder.encode_u32(VgpuPropertiesSchema::ECC_KEY, index, ECC)?;
        encoder.encode_u64(VgpuPropertiesSchema::PROFILE_SIZE_KEY, index, PROFILE_SIZE)?;
        encoder.encode_u32(VgpuPropertiesSchema::MAX_FPS_KEY, index, MAX_FPS)?;
        encoder.encode_u32(VgpuPropertiesSchema::NUM_HEADS_KEY, index, NUM_HEADS)?;
        encoder.encode_u32(VgpuPropertiesSchema::MAX_RES_X_KEY, index, MAX_RES_X)?;
        encoder.encode_u32(VgpuPropertiesSchema::MAX_RES_Y_KEY, index, MAX_RES_Y)?;
        encoder.encode_u32(VgpuPropertiesSchema::DEV_ID_KEY, index, DEV_ID)?;
        encoder.encode_u32(VgpuPropertiesSchema::SUBSYSTEM_ID_KEY, index, SUBSYSTEM_ID)?;
        encoder.encode_u64(VgpuPropertiesSchema::FB_LENGTH_KEY, index, FB_LENGTH)?;
        encoder.encode_u64(
            VgpuPropertiesSchema::GSP_HEAP_SIZE_KEY,
            index,
            GSP_HEAP_SIZE,
        )?;
        encoder.encode_u64(
            VgpuPropertiesSchema::FB_RESERVATION_KEY,
            index,
            FB_RESERVATION,
        )?;
        let data = encoder.finish();

        let decoder = Decoder::new(&data, UnknownKeyPolicy::Ignore);
        let props = KBox::try_init(decoder.decode(VgpuPropertiesSchema::default())?, GFP_KERNEL)?;

        assert_eq!(&*props.name, &name[..]);
        assert_eq!(&*props.class, &class[..]);
        assert_eq!(props.type_id, TYPE_ID);
        assert_eq!(props.bar1_length, BAR1_LENGTH);
        assert_eq!(props.max_instance, MAX_INSTANCE);
        assert_eq!(props.ecc, ECC);
        assert_eq!(props.profile_size, PROFILE_SIZE);
        assert_eq!(props.max_fps, MAX_FPS);
        assert_eq!(props.num_heads, NUM_HEADS);
        assert_eq!(props.max_res_x, MAX_RES_X);
        assert_eq!(props.max_res_y, MAX_RES_Y);
        assert_eq!(props.dev_id, DEV_ID);
        assert_eq!(props.subsystem_id, SUBSYSTEM_ID);
        assert_eq!(props.fb_length, FB_LENGTH);
        assert_eq!(props.gsp_heap_size, GSP_HEAP_SIZE);
        assert_eq!(props.fb_reservation, FB_RESERVATION);

        Ok(())
    }

    #[test]
    fn decode_vgpu_properties_missing_required_fails() -> Result {
        let index = Index::new::<0>();
        let mut encoder = Encoder::new();
        encoder.encode_u32(VgpuPropertiesSchema::ECC_KEY, index, 1)?;
        let data = encoder.finish();

        let decoder = Decoder::new(&data, UnknownKeyPolicy::Ignore);
        let init = decoder.decode(VgpuPropertiesSchema::default())?;
        assert!(KBox::try_init(init, GFP_KERNEL).is_err());

        Ok(())
    }

    #[test]
    fn decode_fb_region_missing_required_fails() -> Result {
        let index = Index::new::<0>();
        let mut encoder = Encoder::new();
        encoder.encode_u64(FbRegionSchema::BASE_KEY, index, 0x1000_0000)?;
        let data = encoder.finish();

        let decoder = Decoder::new(&data, UnknownKeyPolicy::Ignore);
        let init = decoder.decode(FbRegionSchema::default())?;
        assert!(KBox::try_init(init, GFP_KERNEL).is_err());

        Ok(())
    }

    #[test]
    fn gsp_init_response() -> Result {
        let name = b"test name\0";
        const BAR1_PDE_BASE: u64 = 0xdead_0000;
        const FB_REGION0_BASE: u64 = 0x1000_0000;
        const FB_REGION0_LIMIT: u64 = 0x1fff_ffff;
        const FB_REGION0_FLAGS: u32 = 0x7;
        const FB_REGION0_TAG: u32 = 0;
        const FB_REGION1_BASE: u64 = 0x2000_0000;
        const FB_REGION1_LIMIT: u64 = 0x2fff_ffff;
        const FB_REGION1_FLAGS: u32 = 0x3;
        const FB_REGION1_TAG: u32 = 1;
        const VMMU_SEGMENT_SIZE: u64 = 0x0200_0000;

        type Resp = GspInitResponseSchema;

        let index0 = Index::new::<0>();
        let index1 = Index::new::<1>();

        let mut encoder = Encoder::new();
        encoder.encode_u64(Resp::BAR1_PDE_BASE_KEY, index0, BAR1_PDE_BASE)?;
        let data = encoder.finish();

        let decoder = Decoder::new(&data, UnknownKeyPolicy::Ignore);
        let response = KBox::try_init(decoder.decode(Resp::default())?, GFP_KERNEL)?;
        assert_eq!(response.bar1_pde_base, BAR1_PDE_BASE);
        assert!(response.fb_regions.is_empty());

        let mut encoder = Encoder::new();
        encoder.encode_array8(Resp::GPU_NAME_STRING_KEY, index0, name)?;
        encoder.encode_u64(Resp::BAR1_PDE_BASE_KEY, index0, BAR1_PDE_BASE)?;
        encoder.encode_u64(FbRegionSchema::BASE_KEY, index0, FB_REGION0_BASE)?;
        encoder.encode_u64(FbRegionSchema::LIMIT_KEY, index0, FB_REGION0_LIMIT)?;
        encoder.encode_u32(FbRegionSchema::FLAGS_KEY, index0, FB_REGION0_FLAGS)?;
        encoder.encode_u32(FbRegionSchema::TAG_KEY, index0, FB_REGION0_TAG)?;
        encoder.encode_u64(Resp::VMMU_SEGMENT_SIZE_KEY, index0, VMMU_SEGMENT_SIZE)?;
        encoder.encode_u64(FbRegionSchema::BASE_KEY, index1, FB_REGION1_BASE)?;
        encoder.encode_u64(FbRegionSchema::LIMIT_KEY, index1, FB_REGION1_LIMIT)?;
        encoder.encode_u32(FbRegionSchema::FLAGS_KEY, index1, FB_REGION1_FLAGS)?;
        encoder.encode_u32(FbRegionSchema::TAG_KEY, index1, FB_REGION1_TAG)?;
        let data = encoder.finish();

        let decoder = Decoder::new(&data, UnknownKeyPolicy::Error);
        let response = KBox::try_init(decoder.decode(Resp::default())?, GFP_KERNEL)?;

        assert_eq!(&*response.gpu_name, &name[..]);
        assert_eq!(response.bar1_pde_base, BAR1_PDE_BASE);
        assert_eq!(response.fb_regions.len(), 2);
        let fb_region0 = &response.fb_regions[0];
        assert_eq!(fb_region0.base, FB_REGION0_BASE);
        assert_eq!(fb_region0.limit, FB_REGION0_LIMIT);
        assert_eq!(fb_region0.flags.into_raw(), FB_REGION0_FLAGS);
        assert!(fb_region0.flags.support_compressed());
        assert!(fb_region0.flags.support_iso());
        assert!(fb_region0.flags.protected());
        assert_eq!(fb_region0.tag, FB_REGION0_TAG);
        let fb_region1 = &response.fb_regions[1];
        assert_eq!(fb_region1.base, FB_REGION1_BASE);
        assert_eq!(fb_region1.limit, FB_REGION1_LIMIT);
        assert_eq!(fb_region1.flags.into_raw(), FB_REGION1_FLAGS);
        assert_eq!(fb_region1.tag, FB_REGION1_TAG);
        assert_eq!(response.vmmu_segment_size, VMMU_SEGMENT_SIZE);

        Ok(())
    }
}
