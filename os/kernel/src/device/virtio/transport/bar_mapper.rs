/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: bar_mapper.rs                                                   ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ One-time mapping of PCI BARs (Base Address Registers) into kernel space.║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Map PCI BAR memory into virtual memory once per device              ║
   ║   - Maintain a global cache to avoid redundant mappings                 ║
   ║   - Support identity-mapped kernels (phys == virt)                      ║
   ║                                                                         ║
   ║ Assumes page-aligned BAR addresses and uncached device memory mapping.  ║
   ║                                                                         ║
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::collections::BTreeMap;
use alloc::format;
use spin::Mutex;
use spin::rwlock::RwLockWriteGuard;
use x86_64::structures::paging::{Page, PageTableFlags};
use x86_64::structures::paging::page::PageRange;
use x86_64::VirtAddr;

use crate::device::pci::ConfigurationSpace;
use crate::memory::vmm::VmaType;
use crate::memory::MemorySpace;
use crate::process_manager;
use crate::device::virtio::utils::PAGE_SIZE;
use pci_types::{EndpointHeader, PciAddress};

/// Global cache mapping (PCI device address, BAR index) to virtual base address.
static BAR_MAPPINGS: Mutex<BTreeMap<(PciAddress, u8), u64>> = Mutex::new(BTreeMap::new());

/// Map a PCI device's BAR into virtual memory on first request, using identity mapping.
///
/// - Reads the BAR from the device's PCI configuration space.
/// - Calculates the required page range and maps it into the kernel process's address space
///   with PRESENT, WRITABLE, and NO_CACHE flags.
/// - Caches and returns the base virtual address on subsequent calls.
///
/// # Parameters
/// - `pci_config_space`: reference to the PCI configuration space access object.
/// - `pci_device`: locked endpoint header for the target PCI device.
/// - `device_addr`: PCI bus address of the device.
/// - `bar_index`: BAR register index (0..5).
///
/// # Panics
/// - If reading the BAR fails or returns non-memory type.
/// - If the BAR address is not page-aligned.
pub fn map_bar_once(
    pci_config_space: &ConfigurationSpace,
    pci_device: &mut RwLockWriteGuard<EndpointHeader>,
    device_addr: PciAddress,
    bar_index: u8,
) -> u64 {
    let key = (device_addr, bar_index);

    // Return cached mapping if present
    if let Some(&virt_base) = BAR_MAPPINGS.lock().get(&key) {
        return virt_base;
    }

    // Read BAR information (address and size)
    let bar = pci_device
        .bar(bar_index, pci_config_space)
        .expect("Failed to read BAR");
    let (phys_addr, size) = bar.unwrap_mem();

    // Ensure BAR address is page-aligned
    let start_page = Page::from_start_address(VirtAddr::new(phys_addr as u64))
        .expect("BAR address must be page-aligned");

    // Calculate number of pages covering the BAR size
    let num_pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Map physical pages into the kernel's virtual address space
    process_manager()
        .read()
        .kernel_process()
        .expect("Failed to get kernel process")
        .virtual_address_space
        .map(
            PageRange { start: start_page, end: start_page + num_pages as u64 },
            MemorySpace::Kernel,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE,
            VmaType::DeviceMemory,
            &format!("PCI BAR {} mapping", bar_index),
        );

    // Identity mapping: virtual address equals physical
    let virt_base = phys_addr as u64;
    BAR_MAPPINGS.lock().insert(key, virt_base);

    virt_base
}
