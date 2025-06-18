/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: gpu.rs                                                          ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Main implementation of the Virtio GPU device driver.                    ║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Driver initialization, feature negotiation                          ║
   ║   - Virtqueue setup and DMA buffer management                           ║
   ║   - GPU command construction and submission                             ║
   ║   - Framebuffer setup and scanout control                               ║
   ║   - GPU interrupt handler registration                                  ║
   ║                                                                         ║
   ║ References:                                                             ║
   ║   • Virtio Specification 1.3                                            ║
   ║     https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html     ║
   ║   • rCore Virtio driver reference                                       ║
   ║     https://github.com/rcore-os/virtio-drivers/tree/master              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::hint::spin_loop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use log::info;
use bytemuck::{Pod, bytes_of, from_bytes};
use spin::Mutex;

use crate::device::virtio::dma::Dma;
use crate::device::virtio::gpu::protocol::{
    VirtioGpuCtrlHdr, VirtioGpuCtrlType, VirtioGpuRect,
    VirtioGpuResourceAttachBacking, VirtioGpuResourceCreate2d,
    VirtioGpuResourceFlush, VirtioGpuRespDisplayInfo,
    VirtioGpuSetScanout, VirtioGpuTransferToHost2d,
    B8G8R8A8UNORM, CONTROL_QUEUE, CURSOR_QUEUE
};
use crate::device::virtio::transport::features::VirtioFeatures;
use crate::device::virtio::transport::queue::VirtioQueue;
use crate::device::virtio::transport::transport::{DeviceType, Transport};
use crate::device::virtio::utils::{pages, PAGE_SIZE};
use crate::{apic, interrupt_dispatcher};
use crate::device::virtio::gpu::interrupt::VirtioGpuInterruptHandler;
use crate::device::virtio::transport::capabilities::IsrCfg;
use crate::interrupt::interrupt_dispatcher::InterruptVector;

const QUEUE_LEN: u16 = 64;
const DEFAULT_RESOURCE_ID: u32 = 42;

/// Main Virtio GPU device handle with interior mutability.
///
/// Manages feature negotiation, queue setup, and command submission.
/// Based on Virtio 1.3 specification (see §2.4.4 Device Queues).  
pub struct VirtioGpu {
    /// Underlying transport for Virtio communication.
    pub(crate) transport: Mutex<Transport>, 

    /// IRQ vector used during registration.
    pub(crate) interrupt_vector: InterruptVector, 

    /// Interrupt Status Register access.
    pub(crate) isr_cfg: Mutex<IsrCfg>, 

    /// Control queue used for command submission.
    pub(crate) control_queue: Mutex<VirtioQueue<{ QUEUE_LEN as usize }>>, 

    /// Transmit buffer for control commands.
    tx_buffer: Mutex<Box<[u8]>>, 

    /// Receive buffer for control responses.
    rx_buffer: Mutex<Box<[u8]>>, 

    /// Currently active display rectangle for scanout.
    pub(crate) current_rect: Mutex<Option<VirtioGpuRect>>, 

    /// DMA handle for the framebuffer backing.
    pub(crate) framebuffer_dma: Mutex<Option<Dma>>, 

    /// Indicates a configuration change event.
    pub(crate) config_change: AtomicBool, 

    /// Indicates a queue notification interrupt.
    pub(crate) queue_notify: AtomicBool, 
}


impl VirtioGpu {
    /// Discover, negotiate, and initialize the Virtio GPU device.
    ///
    /// # Errors
    /// Returns an error if device probing or initialization fails.
    ///
    /// Corresponds to Virtio 1.3 spec §2.1 Driver Initialization.
    pub fn new() -> Result<Self, String> {
        let mut transport = Transport::new(DeviceType::GPU)?;
        let interrupt_vector = transport.get_interrupt_vector();
        let negotiated = transport.init()?;
        let ctrl_queue = Self::setup_queues(&mut transport, negotiated)?;
        transport.finish_init();
        let isr_cfg = transport.get_isr_cfg();

        Ok(Self {
            transport: Mutex::new(transport),
            interrupt_vector,
            isr_cfg: Mutex::new(isr_cfg),
            control_queue: Mutex::new(ctrl_queue),
            tx_buffer: Mutex::new(Box::new([0u8; PAGE_SIZE])),
            rx_buffer: Mutex::new(Box::new([0u8; PAGE_SIZE])),
            current_rect: Mutex::new(None),
            framebuffer_dma: Mutex::new(None),
            config_change: AtomicBool::new(false),
            queue_notify: AtomicBool::new(false),
        })
    }

    /// Configure the Virtio queues for control and optional cursor.
    ///
    /// # Parameters
    /// - `transport`: the transport to configure queues on.
    /// - `negotiated`: features negotiated with the device.
    ///
    /// # Errors
    /// Returns an error if queue initialization fails or unexpected queue indices.
    ///
    /// See Virtio 1.3 spec §2.4.4 and §5.2 for queue configuration.
    fn setup_queues(
        transport: &mut Transport,
        negotiated: VirtioFeatures,
    ) -> Result<VirtioQueue<{ QUEUE_LEN as usize }>, String> {
        let num_queues = transport.common_cfg.read_num_queues();
        let mut control_queue = None;

        for i in 0..num_queues {
            match i {
                CONTROL_QUEUE => {
                    control_queue = Some(
                        VirtioQueue::<{ QUEUE_LEN as usize }>::new(
                            transport,
                            CONTROL_QUEUE,
                            negotiated.contains(VirtioFeatures::VIRTIO_F_EVENT_IDX),
                        ).map_err(|e| format!("control queue init failed: {:?}", e))?,
                    );
                }
                CURSOR_QUEUE => continue,
                _ => return Err(format!("Unexpected queue index: {}", i)),
            }
        }

        control_queue.ok_or_else(|| "Control queue not initialized".to_string())
    }

    /// Query the primary scanout resolution.
    ///
    /// # Returns
    /// Tuple `(width, height)` in pixels.
    ///
    /// This sends a `GET_DISPLAY_INFO` command (Virtio 1.3 §5.2.3) and reads the first mode.
    pub fn get_resolution(&self) -> Result<(u32, u32), String> {
        let info = self.fetch_display_info()?;
        let mode = &info.pmodes[0];
        info!(
            "Primary scanout active={}, size={}x{}",
            mode.enabled,
            mode.rect.width,
            mode.rect.height
        );
        Ok((mode.rect.width, mode.rect.height))
    }

    /// Initialize and map the framebuffer for software rendering.
    ///
    /// Allocates a 2D resource, attaches DMA backing, and sets up scanout.
    /// Returns a mutable slice to the framebuffer memory.
    pub fn initialize_framebuffer(&self) -> Result<&mut [u8], String> {
        let info = self.fetch_display_info()?;
        let mode = info.pmodes[0];
        *self.current_rect.lock() = Some(mode.rect);

        // Create 2D resource in device
        self.create_2d_resource(DEFAULT_RESOURCE_ID, mode.rect.width, mode.rect.height)?;

        // Allocate host-side backing store via DMA
        let buffer_size = (mode.rect.width * mode.rect.height * 4) as usize;
        let dma_handle = Dma::new(pages(buffer_size));
        self.attach_backing(DEFAULT_RESOURCE_ID, dma_handle.paddr().as_u64(), buffer_size as u32)?;

        // Associate the resource with the scanout
        self.set_scanout_rect(mode.rect, 0, DEFAULT_RESOURCE_ID)?;

        // Store DMA for future flush operations
        let mut fb_dma_slot = self.framebuffer_dma.lock();
        let fb_slice = unsafe { dma_handle.raw_slice().as_mut() };
        *fb_dma_slot = Some(dma_handle);
        Ok(fb_slice)
    }

    /// Flush the entire framebuffer to display via `TRANSFER_TO_HOST2D` and `RESOURCE_FLUSH`.
    pub fn flush_framebuffer(&self) -> Result<(), String> {
        let rect = self.current_rect.lock()
            .clone()
            .ok_or_else(|| "Framebuffer not initialized".to_string())?;

        self.transfer_to_host(rect)?;
        self.flush_resource(rect)?;
        Ok(())
    }

    /// Flush a specific rectangle region of the framebuffer.
    pub fn flush_rect(&self, rect: VirtioGpuRect) -> Result<(), String> {
        self.transfer_to_host(rect)?;
        self.flush_resource(rect)?;
        Ok(())
    }

    /// Internal helper for sending a command and waiting for a response.
    ///
    /// Locks the TX/RX buffers and control queue, submits the descriptor chain,
    /// then waits for a queue notification (interrupt or spin-poll).
    fn send_command_locked<Req: Pod, Resp: Pod>(&self, request: Req) -> Result<Resp, String> {
        let mut tx = self.tx_buffer.lock();
        let mut rx = self.rx_buffer.lock();
        let mut ctrl_q = self.control_queue.lock();
        let mut transport = self.transport.lock();

        // Copy request into TX buffer
        let req_bytes = bytes_of(&request);
        tx[..req_bytes.len()].copy_from_slice(req_bytes);

        // Submit command, get a token for response
        let token = ctrl_q.submit(&[&tx[..req_bytes.len()]], &mut [&mut *rx], &mut *transport)?;

        // Wait for interrupt-driven or polled notification
        while !self.queue_notify.swap(false, Ordering::SeqCst) {
            spin_loop();  // busy-wait; could be replaced by interrupt wait
        }

        // Retrieve the response
        let _ = ctrl_q.receive_answer(token, &[&tx[..req_bytes.len()]], &mut [&mut *rx])?;
        let resp = from_bytes::<Resp>(&rx[..core::mem::size_of::<Resp>()]);
        Ok(*resp)
    }

    /// Fetch display information via `GET_DISPLAY_INFO` command.
    fn fetch_display_info(&self) -> Result<VirtioGpuRespDisplayInfo, String> {
        let resp: VirtioGpuRespDisplayInfo = self.send_command_locked(
            VirtioGpuCtrlHdr::with_ctrl_type(VirtioGpuCtrlType::GET_DISPLAY_INFO),
        )?;
        resp.hdr.check_ctrl_type(VirtioGpuCtrlType::RESP_OK_DISPLAY_INFO)?;
        Ok(resp)
    }

    /// Create a 2D resource on the device (Virtio 1.3 §5.2.4).
    fn create_2d_resource(&self, id: u32, width: u32, height: u32) -> Result<(), String> {
        let hdr: VirtioGpuCtrlHdr = self.send_command_locked(VirtioGpuResourceCreate2d {
            hdr: VirtioGpuCtrlHdr::with_ctrl_type(VirtioGpuCtrlType::RESOURCE_CREATE2D),
            resource_id: id,
            format: B8G8R8A8UNORM,
            width,
            height,
        })?;
        hdr.check_ctrl_type(VirtioGpuCtrlType::RESP_OK_NODATA)
    }

    /// Attach host memory as backing store for a resource (Virtio 1.3 §5.2.5).
    fn attach_backing(&self, id: u32, paddr: u64, length: u32) -> Result<(), String> {
        let hdr: VirtioGpuCtrlHdr = self.send_command_locked(VirtioGpuResourceAttachBacking {
            hdr: VirtioGpuCtrlHdr::with_ctrl_type(VirtioGpuCtrlType::RESOURCE_ATTACH_BACKING),
            resource_id: id,
            nr_entries: 1,
            addr: paddr,
            len: length,
            _padding: 0,
        })?;
        hdr.check_ctrl_type(VirtioGpuCtrlType::RESP_OK_NODATA)
    }

    /// Set resource as the scanout target (Virtio 1.3 §5.2.6).
    fn set_scanout_rect(&self, rect: VirtioGpuRect, scanout: u32, id: u32) -> Result<(), String> {
        let hdr: VirtioGpuCtrlHdr = self.send_command_locked(VirtioGpuSetScanout {
            hdr: VirtioGpuCtrlHdr::with_ctrl_type(VirtioGpuCtrlType::SET_SCANOUT),
            r: rect,
            scanout_id: scanout,
            resource_id: id,
        })?;
        hdr.check_ctrl_type(VirtioGpuCtrlType::RESP_OK_NODATA)
    }

    /// Copy resource data to host for display (Virtio 1.3 §5.2.7).
    fn transfer_to_host(&self, rect: VirtioGpuRect) -> Result<(), String> {
        let hdr: VirtioGpuCtrlHdr = self.send_command_locked(VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr::with_ctrl_type(VirtioGpuCtrlType::TRANSFER_TO_HOST2D),
            r: rect,
            offset: 0,
            resource_id: DEFAULT_RESOURCE_ID,
            _padding: 0,
        })?;
        hdr.check_ctrl_type(VirtioGpuCtrlType::RESP_OK_NODATA)
    }

    /// Flush region to scanout (Virtio 1.3 §5.2.8).
    fn flush_resource(&self, rect: VirtioGpuRect) -> Result<(), String> {
        let hdr: VirtioGpuCtrlHdr = self.send_command_locked(VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr::with_ctrl_type(VirtioGpuCtrlType::RESOURCE_FLUSH),
            r: rect,
            resource_id: DEFAULT_RESOURCE_ID,
            _padding: 0,
        })?;
        hdr.check_ctrl_type(VirtioGpuCtrlType::RESP_OK_NODATA)
    }

    /// Register the interrupt handler for Virtio GPU events.
    pub fn plugin(device: Arc<VirtioGpu>) {
        let interrupt = device.interrupt_vector;
        interrupt_dispatcher().assign(
            interrupt,
            Box::new(VirtioGpuInterruptHandler::new(device)),
        );
        apic().allow(interrupt);
    }
}

//------------------------------------------------------------------------------

/// Memory-mapped device configuration registers for Virtio GPU.
///
/// Provides access to event status and capability counts (Virtio 1.3 §2.3).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GpuConfig {
    /// Bits indicating events that have occurred.
    pub events_read: u32,
    /// Write bits to clear corresponding events.
    pub events_clear: u32,
    /// Number of supported scanout heads.
    pub num_scanouts: u32,
    /// Number of supported capability sets.
    pub num_capsets: u32,
}

/// Safe handle to mapped `GpuConfig` registers.
pub struct GpuCfg {
    regs: NonNull<GpuConfig>,
}

unsafe impl Send for GpuCfg {}
unsafe impl Sync for GpuCfg {}

impl GpuCfg {
    /// # Safety
    /// `base_addr` must point to a valid memory-mapped `virtio_gpu_config` register block.
    pub unsafe fn new(base_addr: *mut GpuConfig) -> Self {
        Self { regs: NonNull::new(base_addr).expect("null pointer to device configuration registers") }
    }

    /// Read current event bits.
    pub fn read_events(&self) -> u32 {
        unsafe { self.regs.as_ref().events_read }
    }

    /// Clear events by writing mask to `events_clear`.
    pub fn clear_events(&self, mask: u32) {
        unsafe { core::ptr::write_volatile(&mut (*self.regs.as_ptr()).events_clear, mask); }
    }

    /// Get the number of scanouts supported.
    pub fn num_scanouts(&self) -> u32 {
        unsafe { self.regs.as_ref().num_scanouts }
    }

    /// Get the number of capability sets supported.
    pub fn num_capsets(&self) -> u32 {
        unsafe { self.regs.as_ref().num_capsets }
    }
}
