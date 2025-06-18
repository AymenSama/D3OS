/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: transport.rs                                                    ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Implements the VirtIO transport layer for PCI-based VirtIO GPU devices. ║
   ║                                                                         ║
   ║ This module detects the VirtIO GPU PCI device, extracts its capabilities║
   ║ (common config, notify config, ISR, device config), negotiates features,║
   ║ and provides methods for queue and interrupt management.                ║
   ║                                                                         ║
   ║ Core Responsibilities:                                                  ║
   ║ - Discover and initialize VirtIO devices on the PCI bus                 ║
   ║ - Negotiate and write driver-supported features                         ║
   ║ - Configure and notify virtqueues                                       ║
   ║ - Provide access to ISR and device-specific configuration               ║
   ║                                                                         ║
   ║ Reference:                                                              ║
   ║   • Virtio Specification 1.3                                            ║
   ║   • PCI VirtIO Transport Spec                                           ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf                                    ║
   ╚═════════════════════════════════════════════════════════════════════════╝ 
  */
use alloc::{string::String, vec::Vec};
use core::fmt;
use log::{error, info};
use pci_types::{EndpointHeader, PciAddress};
use crate::{
    device::virtio::{
        transport::{
            capabilities::{CommonCfg, IsrCfg, NotifyRegion, PciCapability},
            features::{DeviceStatusFlags, VirtioFeatures},
        },
    },
    interrupt::interrupt_dispatcher::InterruptVector,
    pci_bus,
};
use crate::device::virtio::gpu::gpu::GpuCfg;

/// Supported Virtio device types.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceType {
    /// GPU device type identifier.
    GPU = 0x1050,
}

impl fmt::Display for DeviceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceType::GPU => write!(f, "Virtio GPU"),
        }
    }
}

/// Transport abstraction for Virtio GPU over PCI.
pub struct Transport {
    /// The type of virtio device (GPU).
    device_type: DeviceType,
    /// PCI capability base address pointer.
    cap_ptr: PciAddress,

    /// List of Virtio-specific PCI capabilities.
    virtio_caps: Vec<PciCapability>,
    /// Number of Virtio PCI capabilities.
    virtio_caps_count: u32,

    /// Common configuration region.
    pub common_cfg: CommonCfg,
    /// Notification region for queue notifications.
    notify_cfg: NotifyRegion,
    /// Multiplier for notification offsets.
    notify_off_multiplier: u32,
    /// ISR configuration region.
    isr_cfg: IsrCfg,
    /// Device-specific configuration for GPU.
    device_cfg: GpuCfg,
}

impl Transport {
    /// Discover and configure the Virtio GPU PCI device.
    ///
    /// # Errors
    /// Returns an error if no Virtio GPU device is found or PCI capability extraction fails.
    pub fn new(device_type: DeviceType) -> Result<Self, String> {
        let pci_device = match device_type {
            DeviceType::GPU => {
                info!("Searching for Virtio GPU device...");
                let device = Self::search_virtio_device(device_type)?;
                let (vendor_id, device_id) = device.read().header().id(&pci_bus().config_space());
                info!("Found Virtio GPU device: {:X}:{:X}", vendor_id, device_id);
                device
            },
            _ => return Err("Unsupported device type".into()),
        };

        info!("Configuring PCI registers for Virtio {}...", device_type);
        let cap_ptr = pci_device.read().header().address();
        let (
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            isr_cfg,
            device_cfg,
            virtio_caps,
        ) = PciCapability::extract_capabilities(pci_device, cap_ptr, device_type)?;
        let virtio_caps_count = virtio_caps.len() as u32;

        Ok(Transport {
            device_type,
            cap_ptr,
            virtio_caps,
            virtio_caps_count,
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            isr_cfg,
            device_cfg,
        })
    }

    fn search_virtio_device<'a>(device_type: DeviceType) -> Result<&'a spin::RwLock<EndpointHeader>, String> {
        /// PCI Vendor and Device IDs for Virtio GPU.
        const VIRTIO_PCI_VENDOR_ID: u16 = 0x1AF4;
        
        let devices = pci_bus().search_by_ids(VIRTIO_PCI_VENDOR_ID, device_type as u16);
        if let Some(device) = devices.first() {
            Ok(device)
        } else {
            Err("No Virtio GPU device found".into())
        }
    }

    /// Initialize the Virtio GPU device by resetting, acknowledging, and negotiating features.
    ///
    /// # Errors
    /// Returns an error if feature negotiation fails or required features are missing.
    pub fn init(&mut self) -> Result<VirtioFeatures, String> {
        info!("Initializing Virtio driver with id: {}...", self.device_type);

        // Reset device and set initial status bits.
        self.reset_device();
        self.set_status(DeviceStatusFlags::ACKNOWLEDGE | DeviceStatusFlags::DRIVER);

        // Negotiate features with the device.
        let negotiated_features = self.negotiate_features()?;
        self.set_status(
            DeviceStatusFlags::ACKNOWLEDGE
                | DeviceStatusFlags::DRIVER
                | DeviceStatusFlags::FEATURES_OK,
        );

        if !self.is_status_ok(DeviceStatusFlags::FEATURES_OK) {
            error!("Virtio driver with id: {} refused FEATURES_OK", self.device_type);
            self.set_status(DeviceStatusFlags::FAILED);
            return Err("Failed to negotiate features".into());
        }

        Ok(negotiated_features)
    }

    /// Complete driver initialization by finalizing device status.
    pub fn finish_init(&self) {
        self.set_status(
            DeviceStatusFlags::ACKNOWLEDGE
                | DeviceStatusFlags::DRIVER
                | DeviceStatusFlags::FEATURES_OK
                | DeviceStatusFlags::DRIVER_OK,
        );
        if !self.is_status_ok(DeviceStatusFlags::DRIVER_OK) {
            error!("Virtio driver with id: {:?} refused DRIVER_OK", self.device_type);
            self.set_status(DeviceStatusFlags::FAILED);
            return;
        }
        info!("Virtio driver with id: {} initialized successfully!", self.device_type);
    }

    /// Reset the device by clearing its status bits.
    fn reset_device(&self) {
        self.common_cfg.write_device_status(0);
        info!(" -> Virtio device with id: {} reset.", self.device_type);
    }

    /// Update the device status register with new flags.
    fn set_status(&self, status: DeviceStatusFlags) {
        self.common_cfg.write_device_status(status.bits());
        info!(
            " -> Device status set to: {:?}",
            DeviceStatusFlags::from_bits_truncate(self.common_cfg.read_device_status())
        );
    }

    /// Check if the current device status contains the given flags.
    fn is_status_ok(&self, status: DeviceStatusFlags) -> bool {
        self.get_status().contains(status)
    }

    /// Read the current device status flags.
    fn get_status(&self) -> DeviceStatusFlags {
        DeviceStatusFlags::from_bits_truncate(self.common_cfg.read_device_status())
    }

    /// Negotiate Virtio features with the device.
    ///
    /// # Errors
    /// Returns an error if required features (e.g., VIRTIO_F_VERSION_1) are not supported.
    fn negotiate_features(&mut self) -> Result<VirtioFeatures, String> {
        info!("    Negotiating Virtio features...");
        let device_features = self.read_device_features();
        info!("     -> Device features: {:?}", device_features);

        // Require version 1 support.
        if !device_features.contains(VirtioFeatures::VIRTIO_F_VERSION_1) {
            return Err("Virtio device does not support VIRTIO_F_VERSION_1".into());
        }
        let negotiated = VirtioFeatures::VIRTIO_F_VERSION_1;

        // Write negotiated feature set back to the device.
        self.write_driver_features(negotiated.bits());
        info!("     -> Driver features set: {:?}" , negotiated);

        Ok(negotiated)
    }

    /// Read 64-bit feature bitmap from the device.
    fn read_device_features(&self) -> VirtioFeatures {
        // Low 32 bits
        self.common_cfg.write_device_feature_select(0);
        let low = self.common_cfg.read_device_feature();

        // High 32 bits
        self.common_cfg.write_device_feature_select(1);
        let high = self.common_cfg.read_device_feature();

        VirtioFeatures::from_bits_truncate(((high as u64) << 32) | low as u64)
    }

    /// Write 64-bit negotiated features to the device.
    fn write_driver_features(&mut self, features: u64) {
        // Low 32 bits
        self.common_cfg.write_driver_feature_select(0);
        self.common_cfg.write_driver_feature(features as u32);

        // High 32 bits
        self.common_cfg.write_driver_feature_select(1);
        self.common_cfg.write_driver_feature((features >> 32) as u32);
    }

    // --------------------------------------------------------------------------
    // Interrutp management
    // --------------------------------------------------------------------------
    pub fn get_interrupt_vector(&self) -> InterruptVector {
        PciCapability::read_irq(self.cap_ptr)
    }
    
    pub fn get_isr_cfg(&self) -> IsrCfg {
        self.isr_cfg.clone()
    }
    
    // --------------------------------------------------------------------------
    // Queue management
    // --------------------------------------------------------------------------

    /// Check if a Virtio queue is in use (enabled).
    pub fn queue_in_used(&self, queue_index: u16) -> bool {
        self.common_cfg.write_queue_select(queue_index);
        // A value of 1 indicates enabled
        self.common_cfg.read_queue_enable() == 1
    }

    /// Get the size (capacity) of a Virtio queue.
    pub fn get_queue_size(&self, queue_index: u16) -> u32 {
        self.common_cfg.write_queue_select(queue_index);
        self.common_cfg.read_queue_size() as u32
    }

    /// Add and configure a Virtio queue in the device.
    pub fn add_queue_to_device(
        &self,
        queue_index: u16,
        size: u16,
        descriptor_addr: u64,
        driver_area_addr: u64,
        device_area_addr: u64,
    ) {
        self.common_cfg.write_queue_select(queue_index);
        self.common_cfg.write_queue_size(size);
        self.common_cfg.write_queue_desc(descriptor_addr);
        self.common_cfg.write_queue_driver(driver_area_addr);
        self.common_cfg.write_queue_device(device_area_addr);
        self.common_cfg.write_queue_enable(1);
    }

    /// Notify the device that there are new buffers in the given queue.
    pub fn notify(&mut self, queue_index: u16) {
        self.common_cfg.write_queue_select(queue_index);
        let queue_notify_off = self.common_cfg.read_queue_notify_off();
        unsafe {
            self.notify_cfg.notify(queue_index, queue_notify_off, self.notify_off_multiplier);
        }
    }
}
