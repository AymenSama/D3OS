/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: capabilities.rs                                                 ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Extracts and maps Virtio PCI capabilities using vendor-specific         ║
   ║ extensions defined by the Virtio 1.3 specification.                     ║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Traverse the PCI capability list                                    ║
   ║   - Identify and decode Virtio-specific capabilities                    ║
   ║   - Map BAR regions for MMIO register access                            ║
   ║   - Provide typed access to common, notify, ISR, and device config      ║
   ║   - Handle device-specific config (e.g., GPU via `GpuCfg`)              ║
   ║                                                                         ║
   ║ Reference:                                                              ║
   ║   • Virtio Specification 1.3                                            ║
   ║     https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html     ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use crate::device::virtio::transport::bar_mapper::map_bar_once;
use crate::device::virtio::transport::mmio_registers::*;
use crate::interrupt::interrupt_dispatcher::InterruptVector;
use crate::pci_bus;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ptr::{NonNull};
use log::info;
use pci_types::{EndpointHeader, PciAddress};
use spin::RwLock;
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite};
use crate::device::virtio::gpu::gpu::{GpuCfg, GpuConfig};
use crate::device::virtio::transport::transport::DeviceType;

const MAX_VIRTIO_CAPS: usize = 16;
const PCI_CAP_ID_VNDR: u8 = 0x09; // Vendor-Specific
const PCI_CAP_ID_MSIX: u8 = 0x11; // MSI-X

// PCI Capability IDs
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;
const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;
const VIRTIO_PCI_CAP_VENDOR_CFG: u8 = 9;

#[derive(Debug, Clone, Copy)]
pub struct PciCapability {
    /// Generic PCI field: PCI_CAP_ID_VNDR
    pub cap_vndr: u8,
    /// Generic PCI field: next pointer.
    pub cap_next: u8,
    /// Generic PCI field: capability length.
    pub cap_len: u8,
    /// Identifies the structure.
    pub cfg_type: u8,
    /// Where to find it.
    pub bar: u8,
    /// Multiple capabilities of the same type.
    pub id: u8,
    /// Padding to full dword. Not used.
    pub _padding: u8,
    /// Offset within the BAR.
    pub offset: u64,
    /// Length of the structure, in bytes.
    pub length: u64,
}

#[repr(C)]
pub struct CommonCfgRegisters {
    // 32-bit fields
    pub device_feature_select: ReadWrite<u32, DEVICE_FEATURE_SELECT::Register>,
    pub device_feature: ReadOnly<u32, DEVICE_FEATURE::Register>,
    pub driver_feature_select: ReadWrite<u32, DRIVER_FEATURE_SELECT::Register>,
    pub driver_feature: ReadWrite<u32, DRIVER_FEATURE::Register>,

    // 16-bit fields
    pub config_msix_vector: ReadWrite<u16, CONFIG_MSIX_VECTOR::Register>,
    pub num_queues: ReadOnly<u16, NUM_QUEUES::Register>,

    // 8-bit fields
    pub device_status: ReadWrite<u8, DEVICE_STATUS::Register>,
    pub config_generation: ReadOnly<u8, CONFIG_GENERATION::Register>,

    // Queue-specific 16-bit fields
    pub queue_select: ReadWrite<u16, QUEUE_SELECT::Register>,
    pub queue_size: ReadWrite<u16, QUEUE_SIZE::Register>,
    pub queue_msix_vector: ReadWrite<u16, QUEUE_MSIX_VECTOR::Register>,
    pub queue_enable: ReadWrite<u16, QUEUE_ENABLE::Register>,
    pub queue_notify_off: ReadOnly<u16, QUEUE_NOTIFY_OFF::Register>,
    //pub queue_notify_data: ReadOnly<u16, QUEUE_NOTIFY_DATA::Register>,
    //pub queue_reset: ReadWrite<u16, QUEUE_RESET::Register>,

    // Queue-specific 64-bit address fields
    pub queue_desc: ReadWrite<u64, QUEUE_DESC::Register>,
    pub queue_driver: ReadWrite<u64, QUEUE_DRIVER::Register>,
    pub queue_device: ReadWrite<u64, QUEUE_DEVICE::Register>,
}

pub struct CommonCfg {
    regs: NonNull<CommonCfgRegisters>,
}

impl CommonCfg {
    /// # Safety
    /// `base_addr` must point to a valid memory-mapped device register block.
    pub unsafe fn new(base_addr: *mut CommonCfgRegisters) -> Self {
        Self {
            regs: NonNull::new(base_addr).expect("null pointer to device registers"),
        }
    }

    #[inline]
    fn regs(&self) -> &CommonCfgRegisters {
        unsafe { self.regs.as_ref() }
    }

    // 32-bit register accessors
    pub fn read_device_feature_select(&self) -> u32 {
        self.regs()
            .device_feature_select
            .read(DEVICE_FEATURE_SELECT::VALUE)
    }
    pub fn write_device_feature_select(&self, value: u32) {
        self.regs()
            .device_feature_select
            .write(DEVICE_FEATURE_SELECT::VALUE.val(value));
    }
    pub fn read_device_feature(&self) -> u32 {
        self.regs().device_feature.read(DEVICE_FEATURE::VALUE)
    }
    pub fn read_driver_feature_select(&self) -> u32 {
        self.regs()
            .driver_feature_select
            .read(DRIVER_FEATURE_SELECT::VALUE)
    }
    pub fn write_driver_feature_select(&self, value: u32) {
        self.regs()
            .driver_feature_select
            .write(DRIVER_FEATURE_SELECT::VALUE.val(value));
    }
    pub fn read_driver_feature(&self) -> u32 {
        self.regs().driver_feature.read(DRIVER_FEATURE::VALUE)
    }
    pub fn write_driver_feature(&self, value: u32) {
        self.regs()
            .driver_feature
            .write(DRIVER_FEATURE::VALUE.val(value));
    }

    // 16-bit register accessors
    pub fn read_config_msix_vector(&self) -> u16 {
        self.regs()
            .config_msix_vector
            .read(CONFIG_MSIX_VECTOR::VALUE)
    }
    pub fn write_config_msix_vector(&self, value: u16) {
        self.regs()
            .config_msix_vector
            .write(CONFIG_MSIX_VECTOR::VALUE.val(value));
    }
    pub fn read_num_queues(&self) -> u16 {
        self.regs().num_queues.read(NUM_QUEUES::VALUE)
    }

    // 8-bit register accessors
    pub fn read_device_status(&self) -> u8 {
        self.regs().device_status.read(DEVICE_STATUS::VALUE)
    }
    pub fn write_device_status(&self, value: u8) {
        self.regs()
            .device_status
            .write(DEVICE_STATUS::VALUE.val(value));
    }
    pub fn read_config_generation(&self) -> u8 {
        self.regs().config_generation.read(CONFIG_GENERATION::VALUE)
    }

    // Queue-specific 16-bit accessors
    pub fn read_queue_select(&self) -> u16 {
        self.regs().queue_select.read(QUEUE_SELECT::VALUE)
    }
    pub fn write_queue_select(&self, value: u16) {
        self.regs()
            .queue_select
            .write(QUEUE_SELECT::VALUE.val(value));
    }
    pub fn read_queue_size(&self) -> u16 {
        self.regs().queue_size.read(QUEUE_SIZE::VALUE)
    }
    pub fn write_queue_size(&self, value: u16) {
        self.regs().queue_size.write(QUEUE_SIZE::VALUE.val(value));
    }
    pub fn read_queue_msix_vector(&self) -> u16 {
        self.regs().queue_msix_vector.read(QUEUE_MSIX_VECTOR::VALUE)
    }
    pub fn write_queue_msix_vector(&self, value: u16) {
        self.regs()
            .queue_msix_vector
            .write(QUEUE_MSIX_VECTOR::VALUE.val(value));
    }
    pub fn read_queue_enable(&self) -> u16 {
        self.regs().queue_enable.read(QUEUE_ENABLE::VALUE)
    }
    pub fn write_queue_enable(&self, value: u16) {
        self.regs()
            .queue_enable
            .write(QUEUE_ENABLE::VALUE.val(value));
    }
    pub fn read_queue_notify_off(&self) -> u16 {
        self.regs().queue_notify_off.read(QUEUE_NOTIFY_OFF::VALUE)
    }
    /*pub fn read_queue_notify_data(&self) -> u16 {
        self.regs().queue_notify_data.read(QUEUE_NOTIFY_DATA::VALUE)
    }
    pub fn read_queue_reset(&self) -> u16 {
        self.regs().queue_reset.read(QUEUE_RESET::VALUE)
    }
    pub fn write_queue_reset(&self, value: u16) {
        self.regs().queue_reset.write(QUEUE_RESET::VALUE.val(value));
    }*/

    // Queue-specific 64-bit accessors
    pub fn read_queue_desc(&self) -> u64 {
        self.regs().queue_desc.read(QUEUE_DESC::VALUE)
    }
    pub fn write_queue_desc(&self, value: u64) {
        self.regs().queue_desc.write(QUEUE_DESC::VALUE.val(value));
    }
    pub fn read_queue_driver(&self) -> u64 {
        self.regs().queue_driver.read(QUEUE_DRIVER::VALUE)
    }
    pub fn write_queue_driver(&self, value: u64) {
        self.regs()
            .queue_driver
            .write(QUEUE_DRIVER::VALUE.val(value));
    }
    pub fn read_queue_device(&self) -> u64 {
        self.regs().queue_device.read(QUEUE_DEVICE::VALUE)
    }
    pub fn write_queue_device(&self, value: u64) {
        self.regs()
            .queue_device
            .write(QUEUE_DEVICE::VALUE.val(value));
    }
}

unsafe impl Send for CommonCfg {}
unsafe impl Sync for CommonCfg {}

#[derive(Debug)]
pub struct NotifyRegion {
    regs: NonNull<u16>,
}

impl NotifyRegion {
    /// Creates a new NotifyRegion from the given base pointer.
    /// The pointer must be non-null and aligned to a u16 boundary.
    pub fn new(ptr: *mut u16) -> Self {
        Self {
            regs: NonNull::new(ptr).expect("Notify region pointer is null"),
        }
    }

    /// Notifies the device for the given queue.
    ///
    /// The caller must provide:
    /// - `queue`: the queue number.
    /// - `queue_notify_off`: the notify offset (read from the common config register).
    /// - `notify_off_multiplier`: the multiplier from the notify capability.
    ///
    /// This computes the proper u16 index in the notify register array
    /// and writes the queue number using a volatile write.
    pub unsafe fn notify(&self, queue: u16, queue_notify_off: u16, notify_off_multiplier: u32) {
        let offset_bytes = queue_notify_off as usize * notify_off_multiplier as usize;
        let index = offset_bytes / size_of::<u16>();
        let target = self.regs.as_ptr().add(index);
        core::ptr::write_volatile(target, queue);
    }
}

unsafe impl Send for NotifyRegion {}
unsafe impl Sync for NotifyRegion {}

/// Define ISR configuration registers as specified in the Virtio PCI spec.
#[repr(C)]
pub struct IsrCfgRegisters {
    /// ISR status field (read-only for driver)
    pub isr_status: u8,
    _padding: [u8; 3],
}

#[derive(Clone)]
pub struct IsrCfg {
    regs: NonNull<IsrCfgRegisters>,
}

unsafe impl Send for IsrCfg {}
unsafe impl Sync for IsrCfg {}

impl IsrCfg {
    /// # Safety
    /// `base_addr` must point to a valid ISR configuration register block.
    pub unsafe fn new(base_addr: *mut IsrCfgRegisters) -> Self {
        Self {
            regs: NonNull::new(base_addr).expect("null pointer to ISR config registers"),
        }
    }

    /// Reads the ISR status.
    pub fn read_status(&self) -> u8 {
        unsafe { self.regs.as_ref().isr_status }
    }

    /// Returns the raw pointer (for direct MMIO read without mutex)
    pub fn base_addr(&self) -> *mut u8 {
        self.regs.as_ptr() as *mut u8
    }
}

impl PciCapability {
    /// Extracts Virtio-specific PCI capabilities from the PCI configuration space.
    ///
    /// Parses the PCI capability list and identifies all vendor-specific capabilities.
    /// Each capability is interpreted according to its `cfg_type`.
    /// The method maps memory regions (via BARs) and initializes MMIO access structures.
    ///
    /// Returns:
    /// - CommonCfg: for feature negotiation and queue configuration
    /// - NotifyRegion: for queue notification
    /// - notify_off_multiplier: multiplier for computing notify offset
    /// - IsrCfg: for reading the interrupt status
    /// - GpuCfg: device-specific register block for GPU
    /// - A list of all discovered PciCapability structs
    ///
    /// # Errors
    /// - Returns an error if any required capability is missing or if an unknown device type is encountered.
    pub fn extract_capabilities(
        pci_device: &RwLock<EndpointHeader>,
        address: PciAddress,
        device_type: DeviceType,
    ) -> Result<(CommonCfg, NotifyRegion, u32, IsrCfg, GpuCfg, Vec<PciCapability>), String> {
        const PCI_STATUS_OFFSET: u16 = 0x06;
        const PCI_STATUS_CAP_LIST: u16 = 1 << 4;
        const PCI_CAP_POINTER_OFFSET: u16 = 0x34;

        let mut common_cfg = None;
        let mut notify_region = None;
        let mut notify_off_multiplier = 0;
        let mut isr_cfg = None;
        let mut device_cfg = None;
        let mut capabilities = Vec::new();

        let config_space = pci_bus().config_space();
        let mut pci_device = pci_device.write();

        // Check if the capabilities list is present in PCI status register
        let status = config_space.read_u16(address, PCI_STATUS_OFFSET);
        if status & PCI_STATUS_CAP_LIST == 0 {
            return Err("No capabilities found".to_string());
        }

        // Begin walking the linked list of PCI capabilities
        let mut cap_ptr = config_space.read_u8(address, PCI_CAP_POINTER_OFFSET);
        while cap_ptr != 0 {
            let base = cap_ptr as u16;
            let cap_id = config_space.read_u8(address, base + 0);
            let cap_next = config_space.read_u8(address, base + 1);
            let cap_len = config_space.read_u8(address, base + 2);

            // Only handle vendor-specific capabilities (Virtio uses these)
            if cap_id == PCI_CAP_ID_VNDR {
                let cfg_type = config_space.read_u8(address, base + 3);
                let bar = config_space.read_u8(address, base + 4);
                let offset_lo = config_space.read_u32(address, base + 8) as u64;
                let length_lo = config_space.read_u32(address, base + 12) as u64;

                let mut offset = offset_lo;
                let mut length = length_lo;

                // If the capability is 64-bit, read upper halves of offset and length
                if cap_len >= 24 {
                    let offset_hi = config_space.read_u32(address, base + 16) as u64;
                    let length_hi = config_space.read_u32(address, base + 20) as u64;
                    offset |= offset_hi << 32;
                    length |= length_hi << 32;
                }

                // Create PciCapability struct for logging or later use
                let capability = PciCapability {
                    cap_vndr: cap_id,
                    cap_next,
                    cap_len,
                    cfg_type,
                    bar,
                    id: 0,
                    _padding: 0,
                    offset,
                    length,
                };
                capabilities.push(capability);

                // Match capability type and map/register accordingly
                match cfg_type {
                    VIRTIO_PCI_CAP_COMMON_CFG => {
                        let virt_base = map_bar_once(config_space, &mut pci_device, address, bar);
                        let common_cfg_ptr = (virt_base + offset) as *mut CommonCfgRegisters;
                        common_cfg = Some(unsafe { CommonCfg::new(common_cfg_ptr) });
                        info!(
                            "Found common configuration capability at bar: {}, offset: {}",
                            bar, offset
                        );
                    }
                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                        notify_off_multiplier = config_space.read_u32(address, (base + 16) as u16);
                        let virt_base = map_bar_once(config_space, &mut pci_device, address, bar);
                        let notify_ptr = (virt_base + offset) as *mut u16;
                        notify_region = Some(NotifyRegion::new(notify_ptr));
                        info!(
                            "Found notify configuration capability at bar: {}, offset: {}, notify_off_multiplier: {}",
                            bar,
                            offset,
                            notify_off_multiplier
                        );
                    }
                    VIRTIO_PCI_CAP_ISR_CFG => {
                        let virt_base = map_bar_once(config_space, &mut pci_device, address, bar);
                        let isr_cfg_ptr = (virt_base + offset) as *mut IsrCfgRegisters;
                        isr_cfg = Some(unsafe { IsrCfg::new(isr_cfg_ptr) });
                        info!(
                            "Found ISR configuration capability at bar: {}, offset: {}",
                            bar,
                            offset
                        );
                    }
                    VIRTIO_PCI_CAP_DEVICE_CFG => {
                        let virt_base = map_bar_once(config_space, &mut pci_device, address, bar);
                        match device_type {
                            DeviceType::GPU => {
                                let device_cfg_ptr = (virt_base + offset) as *mut GpuConfig;
                                device_cfg = Some(unsafe { GpuCfg::new(device_cfg_ptr) });
                            }
                            _ => {
                                return Err("Unsupported device type for device configuration".to_string());
                            }
                        }
                        info!(
                            "Found device configuration capability at bar: {}, offset: {}",
                            bar, offset
                        );
                    }
                    _ => {
                        // Any other unknown vendor-specific capability
                        info!(
                            "Found unknown capability with cfg_type: {}, bar: {}, offset: {}",
                            cfg_type, bar, offset
                        );
                    }
                }
            }

            // Advance to next capability in the PCI capability list
            cap_ptr = cap_next;
        }

        // Ensure all required capabilities were found
        Ok((
            common_cfg.ok_or("Common configuration not found")?,
            notify_region.ok_or("Notify configuration not found")?,
            notify_off_multiplier,
            isr_cfg.ok_or("ISR configuration not found")?,
            device_cfg.ok_or("Device configuration not found")?,
            capabilities,
        ))
    }

    pub fn read_irq(device_address: PciAddress) -> InterruptVector {
        // The legacy interrupt line is typically stored at offset 0x3C in the PCI config header.
        let config_space = pci_bus().config_space();
        const PCI_INTERRUPT_LINE_OFFSET: u16 = 0x3C;
        let irq = config_space.read_u8(device_address, PCI_INTERRUPT_LINE_OFFSET);
        InterruptVector::try_from(irq + 32).unwrap()
    }
}
