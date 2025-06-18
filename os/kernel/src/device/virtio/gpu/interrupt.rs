/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: interrupt.rs                                                    ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Interrupt handler for Virtio GPU device events.                         ║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Read and interpret Virtio ISR status bits                           ║
   ║   - Signal pending events to the GPU driver                             ║
   ║   - Set `queue_notify` and `config_change` flags                        ║
   ║                                                                         ║
   ║ Reference:                                                              ║
   ║   • Virtio Specification 1.3, §2.5 – Interrupts and ISR                 ║
   ║     https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html     ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use crate::device::virtio::gpu::gpu::VirtioGpu;
use crate::interrupt::interrupt_handler::InterruptHandler;

/// Handles Virtio GPU interrupts by reading the ISR status and
/// setting corresponding flags on the device.
///
/// The ISR register provides two event types:
/// - bit 0: Queue interrupt (used to signal used buffers)
/// - bit 1: Device configuration change
pub struct VirtioGpuInterruptHandler {
    /// Shared reference to the Virtio GPU device
    device: Arc<VirtioGpu>,
}

impl VirtioGpuInterruptHandler {
    /// Create a new interrupt handler for the given Virtio GPU device.
    ///
    /// # Parameters
    /// - `device`: An `Arc` pointing to the initialized `VirtioGpu` instance.
    pub fn new(device: Arc<VirtioGpu>) -> Self {
        Self { device }
    }
}

impl InterruptHandler for VirtioGpuInterruptHandler {
    /// Triggered when the assigned IRQ fires.
    ///
    /// Reads the ISR status register, then updates the device's
    /// `queue_notify` or `config_change` flags accordingly.
    fn trigger(&self) {
        // Read ISR status (volatile to prevent compiler reorders)
        let status = self.device.isr_cfg.lock().read_status();

        // If a queue interrupt is pending, signal the device
        if status & 0x1 != 0 {
            self.device.queue_notify.store(true, Ordering::SeqCst);
        }

        // If a configuration change interrupt is pending, signal the device
        if status & 0x2 != 0 {
            self.device.config_change.store(true, Ordering::SeqCst);
        }
    }
}
