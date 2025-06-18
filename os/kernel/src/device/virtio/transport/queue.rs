/* ╔═════════════════════════════════════════════════════════════════════════════════════╗
   ║ Module: queue.rs                                                                    ║
   ╟─────────────────────────────────────────────────────────────────────────────────────╢
   ║ Implements the VirtIO queue logic, including descriptor handling and ring buffers.  ║
   ║                                                                                     ║
   ║ - VirtioQueue: main structure managing descriptor allocation, buffer submission,    ║
   ║   and used ring processing.                                                         ║
   ║ - Descriptor: represents a DMA-capable buffer with chaining and direction flags.    ║
   ║ - AvailableRing / UsedRing: shared memory structures for communication between      ║
   ║   driver and device.                                                                ║
   ║ - Fully supports split virtqueue layout as per VirtIO 1.3.                          ║
   ║                                                                                     ║
   ║ Reference:                                                                          ║
   ║   • Virtio Specification 1.3                                                        ║
   ║     https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html                 ║
   ║   • rCore VirtIO GPU queue implementation                                           ║
   ║     https://github.com/rcore-os/virtio-drivers/blob/master/src/device/gpu.rs        ║
   ╟─────────────────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                                          ║
   ╚═════════════════════════════════════════════════════════════════════════════════════╝
*/
use alloc::format;
use alloc::string::{String};
use core::mem::{size_of};
use core::ptr::{NonNull};
use core::sync::atomic::{AtomicU16, Ordering, fence};
use log::info;

use crate::device::virtio::dma::Dma;
use crate::device::virtio::transport::transport::Transport;
use crate::device::virtio::utils::pages;

/// -----------------------------------------------------------------
/// Descriptor Flags and Descriptor Table
/// -----------------------------------------------------------------

/// Flags associated with a descriptor. Uses a transparent wrapper for better type safety.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct DescriptorFlags(u16);

bitflags::bitflags! {
    impl DescriptorFlags: u16 {
        /// Indicates that the descriptor chain continues in another descriptor.
        const VIRTQ_DESC_F_NEXT     = 1;
        /// Indicates that the buffer is writable by the device.
        const VIRTQ_DESC_F_WRITE    = 2;
    }
}

/// Descriptor structure as defined in Virtio Spec 2.7.5.
/// Note the alignment requirement imposed by #[repr(C, align(16))] to ensure proper memory layout.
#[repr(C, align(16))]
#[derive(Clone, Debug, Copy)]
pub struct Descriptor {
    /// Physical address of the buffer.
    address: u64,
    /// Length of the buffer.
    length: u32,
    /// Flags indicating properties of the descriptor.
    flags: DescriptorFlags,
    /// The next descriptor in the chain (if any).
    next: u16,
}

impl Descriptor {
    /// Sets the DMA buffer for this descriptor.
    ///
    /// # Safety
    /// Caller must ensure the buffer is valid for DMA and will remain
    /// valid while the descriptor is in use by the device.
    ///
    /// # Parameters
    /// - `buffer`: A non-null pointer to a slice representing the buffer.
    /// - `flags`: Descriptor flags indicating properties such as write access
    ///   or chaining to the next descriptor.
    pub unsafe fn set_buffer(
        &mut self,
        buffer: NonNull<[u8]>,
        flags: DescriptorFlags,
    ) {
        // Convert the virtual address to a physical (DMA) address.
        self.address = Dma::get_pointer_to_vaddr(buffer) as u64;

        // Set the byte length of the buffer.
        self.length = buffer.len().try_into().unwrap();

        // Store the descriptor flags (e.g., VIRTQ_DESC_F_WRITE, VIRTQ_DESC_F_NEXT).
        self.flags = flags;
    }

    /// Returns the index of the next descriptor in the chain, if any.
    ///
    /// # Returns
    /// - `Some(index)` if `VIRTQ_DESC_F_NEXT` is set.
    /// - `None` if this is the last descriptor in the chain.
    pub fn next(&self) -> Option<u16> {
        if self.flags.contains(DescriptorFlags::VIRTQ_DESC_F_NEXT) {
            Some(self.next)
        } else {
            None
        }
    }

    /// Clears the buffer information from this descriptor.
    ///
    /// This resets the address and length to zero, effectively making
    /// the descriptor unused. Does not alter `flags` or `next`.
    pub fn unset_buffer(&mut self) {
        self.address = 0;
        self.length = 0;
    }
}

/// -----------------------------------------------------------------
/// Available and Used Rings
/// -----------------------------------------------------------------

/// Structure representing the available ring (driver-to-device communication) as defined in Virtio Spec Section 2.7.6.
#[repr(C)]
#[derive(Debug)]
pub struct AvailableRing<const SIZE: usize> {
    /// Flags that control ring behavior.
    flags: AtomicU16,
    /// Index to the next free slot.
    index: AtomicU16,
    /// Ring buffer holding descriptor indices.
    ring: [u16; SIZE],
    /// Used only if VIRTIO_F_EVENT_IDX has been negotiated.
    used_event: AtomicU16,
}

/// Structure representing the used ring (device-to-driver communication).
#[repr(C)]
#[derive(Debug)]
pub struct UsedRing<const SIZE: usize> {
    /// Flags controlling used ring notifications.
    flags: AtomicU16,
    /// Index to the next element in the used ring.
    index: AtomicU16,
    /// Ring buffer holding the used descriptor elements.
    ring: [UsedRingElement; SIZE],
    /// Used only if VIRTIO_F_EVENT_IDX has been negotiated.
    avail_event: AtomicU16,
}

/// An element inside the used ring holding the descriptor ID and the length processed by the device.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct UsedRingElement {
    /// Identifier of the descriptor.
    id: u32,
    /// The length of data processed.
    length: u32,
}

/// -----------------------------------------------------------------
/// Queue Layout
/// -----------------------------------------------------------------

/// Layout for the Virtio Queue, holding the DMA regions for both driver-to-device and device-to-driver rings.
pub struct VirtQueueLayout {
    /// DMA mapping for the descriptor table and available ring.
    desc_avail_dma: Dma,
    /// DMA mapping for the used ring.
    used_dma: Dma,
    /// Offset in bytes to the available ring region within the combined DMA block.
    avail_offset: usize,
}

impl VirtQueueLayout {
    /// Allocates the layout for a Virtio queue.
    ///
    /// # Parameters
    /// - `queue_size`: The number of descriptors in the queue.
    ///
    /// # Returns
    /// - `Ok(Self)` with the allocated DMA areas.
    /// - `Err(String)` if there is an error during allocation.
    fn allocate_layout(queue_size: u16) -> Result<Self, String> {
        // Compute sizes for the descriptor table, available ring, and used ring.
        let (desc_size, avail_size, used_size) = get_queue_sizes(queue_size);
        // Allocate DMA memory for the driver-to-device area (descriptors + avail ring).
        let driver_to_device_dma = Dma::new(pages(desc_size + avail_size));
        // Allocate DMA memory for the used ring.
        let device_to_driver_dma = Dma::new(pages(used_size));
        Ok(VirtQueueLayout {
            desc_avail_dma: driver_to_device_dma,
            used_dma: device_to_driver_dma,
            avail_offset: desc_size,
        })
    }
}

/// -----------------------------------------------------------------
/// VirtioQueue: Definition and Implementation
/// -----------------------------------------------------------------

/// Main structure representing a Virtio queue with its associated rings and state.
pub struct VirtioQueue<const SIZE: usize> {
    /// DMA layout for the queue.
    layout: VirtQueueLayout,

    /// Pointer to the shared descriptor table.
    descriptors: NonNull<[Descriptor]>,
    /// Pointer to the available ring.
    available: NonNull<AvailableRing<SIZE>>,
    /// Pointer to the used ring.
    used: NonNull<UsedRing<SIZE>>,

    /// Local backup copy of the descriptors.
    descriptor_backup: [Descriptor; SIZE],

    // Queue state
    /// The queue's index within the device.
    queue_index: u16,
    /// Number of descriptors currently in use.
    used_count: u16,
    /// Head index of the free descriptor chain.
    free_descriptor_head: u16,
    /// Current index in the available ring.
    available_index: u16,
    /// Last processed index in the used ring.
    last_used_ring_index: u16,

    /// Flag indicating if event index notifications are enabled.
    event_index_enabled: bool,
}

unsafe impl<const SIZE: usize> Send for VirtioQueue<SIZE> {}
unsafe impl<const SIZE: usize> Sync for VirtioQueue<SIZE> {}

impl<const SIZE: usize> VirtioQueue<SIZE> {
    /// Creates and initializes a new `VirtioQueue` instance with a fixed layout.
    ///
    /// This function allocates DMA memory for the queue's descriptor table, available ring,
    /// and used ring. It informs the VirtIO device of the queue's memory locations and sets
    /// up internal tracking structures for descriptor management.
    ///
    /// # Parameters
    /// - `transport`: A mutable reference to the `Transport` abstraction that interacts
    ///   with the VirtIO device's MMIO/PCI configuration space.
    /// - `queue_index`: The index of the virtqueue (e.g., 0 for control queue, 1 for cursor queue).
    /// - `event_index_enabled`: Whether the `VIRTIO_F_EVENT_IDX` feature is enabled for this queue.
    ///
    /// # Returns
    /// A `Result` containing a fully initialized `VirtioQueue` on success, or an error string
    /// if initialization fails (e.g., queue already used or size mismatch).
    ///
    /// # Specification Reference
    /// - VirtIO Spec 1.3, §2.6: Virtqueues and Ring Layout
    /// - DMA memory layout must follow: descriptor table → available ring → (padding) → used ring
    pub fn new(
        transport: &mut Transport,
        queue_index: u16,
        event_index_enabled: bool,
    ) -> Result<Self, String> {
        // Ensure the queue isn't already in use on the device.
        if transport.queue_in_used(queue_index) {
            return Err("Queue is already in use".into());
        }

        // Verify that the queue size reported by the device matches our expected constant.
        if transport.get_queue_size(queue_index) < SIZE as u32 {
            info!("Queue size {} is not supported", SIZE);
            return Err("Queue size is not supported".into());
        }

        let size = SIZE as u16;

        // Allocate the full DMA layout: descriptor table + available ring + used ring.
        let layout = VirtQueueLayout::allocate_layout(size)?;

        // Inform the device about the queue's DMA configuration.
        transport.add_queue_to_device(
            queue_index,
            size.into(),
            layout.desc_avail_dma.paddr().as_u64(),                    // Descriptor table address
            layout.desc_avail_dma.paddr().as_u64() + layout.avail_offset as u64, // Available ring address
            layout.used_dma.paddr().as_u64(),                          // Used ring address
        );

        // Descriptor table: array of `Descriptor` entries.
        let descriptors = NonNull::slice_from_raw_parts(
            layout.desc_avail_dma.vaddr(0).cast::<Descriptor>(),
            SIZE,
        );

        // Available ring: contains indices of descriptors ready for the device.
        let available = layout.desc_avail_dma.vaddr(layout.avail_offset).cast();

        // Used ring: filled by the device after processing buffers.
        let used = layout.used_dma.vaddr(0).cast();

        // Initialize freelist for descriptor management (circular list of unused descriptors).
        let descriptor_backup = Self::initialize_descriptor_freelist(size, descriptors);

        // Construct and return the queue instance.
        Ok(VirtioQueue {
            layout,
            descriptors,
            available,
            used,
            descriptor_backup,
            queue_index,
            used_count: 0,
            free_descriptor_head: 0,
            available_index: 0,
            last_used_ring_index: 0,
            event_index_enabled,
        })
    }


    /// Initializes a freelist of descriptors by linking them via the `next` field.
    ///
    /// This sets up both the in-memory backup and the device-visible descriptor table
    /// as a singly linked list of free descriptors.
    ///
    /// # Arguments
    /// - `size`: Total number of descriptors in the queue.
    /// - `descriptors`: Pointer to the descriptor table in DMA memory.
    ///
    /// # Returns
    /// A zero-initialized array of `Descriptor` structs with proper freelist links.
    fn initialize_descriptor_freelist(
        size: u16,
        descriptors: NonNull<[Descriptor]>,
    ) -> [Descriptor; SIZE] {
        // Create a local zeroed descriptor backup array.
        let mut descriptor_backup: [Descriptor; SIZE] = [Descriptor {
            address: 0,
            length: 0,
            flags: DescriptorFlags::empty(),
            next: 0,
        }; SIZE];

        // Set up the freelist: link each descriptor to the next.
        for i in 0..(size - 1) {
            let idx = i as usize;
            descriptor_backup[idx].next = i + 1;
            descriptor_backup[idx].unset_buffer();

            unsafe {
                let desc = &mut (*descriptors.as_ptr())[idx];
                desc.next = i + 1;
                desc.flags = DescriptorFlags::empty();
                desc.length = 0;
                desc.address = 0;
            }
        }

        descriptor_backup
    }


    /// Submits a set of input/output buffers to the virtqueue for device processing.
    ///
    /// This method allocates a chain of descriptors from the freelist, writes the given
    /// input (read-only) and output (writable) buffers into the descriptor table,
    /// inserts the head of the chain into the available ring, and updates the index
    /// to signal availability to the device.
    ///
    /// # Arguments
    /// - `inputs`: Slice of immutable byte slices. These are read by the device.
    /// - `outputs`: Slice of mutable byte slices. These are written by the device.
    ///
    /// # Returns
    /// - `Ok(head)`: The index of the first descriptor in the chain, ready for tracking.
    /// - `Err`: If no buffers are provided or not enough free descriptors exist.
    pub unsafe fn add<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16, String> {
        // Reject if no buffers were given.
        if inputs.is_empty() && outputs.is_empty() {
            return Err("No buffers provided".into());
        }

        // Check if enough descriptors are available.
        let total = inputs.len() + outputs.len();
        let available = SIZE - self.used_count as usize;
        if total > available {
            return Err(format!(
                "Insufficient descriptors: need {}, have {}",
                total, available
            ));
        }

        // Allocate and chain descriptors for the buffers.
        let head = self.add_buffers_direct(inputs, outputs);

        // Insert the head descriptor into the available ring.
        let avail_ring = &mut *self.available.as_ptr();
        let slot = (self.available_index & (SIZE as u16 - 1)) as usize;
        avail_ring.ring[slot] = head;

        // Ensure descriptor writes are visible before notifying the device.
        fence(Ordering::Release);

        // Advance the available index and publish it to the device.
        self.available_index = self.available_index.wrapping_add(1);
        avail_ring.index.store(self.available_index, Ordering::Release);

        Ok(head)
    }

    /// Allocates and links descriptors for the given input/output buffers.
    ///
    /// Buffers are pulled from the freelist (`free_descriptor_head`) and written into the
    /// descriptor table and backup copy. All descriptors are chained using the `next` field,
    /// with the last descriptor's `NEXT` flag cleared to terminate the chain.
    ///
    /// # Arguments
    /// - `inputs`: Slice of read-only buffers to be sent to the device.
    /// - `outputs`: Slice of writable buffers for the device to write results into.
    ///
    /// # Returns
    /// The index of the first descriptor in the chain (head), which is then inserted
    /// into the available ring by the caller.
    fn add_buffers_direct<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> u16 {
        let head = self.free_descriptor_head;
        let mut last = head;

        // Helper closure to set up one descriptor.
        let mut assign_buffer = |buf: NonNull<[u8]>, flags: DescriptorFlags| {
            let idx = self.free_descriptor_head as usize;
            let desc = &mut self.descriptor_backup[idx];

            // Write buffer address, length, and flags.
            unsafe {
                desc.set_buffer(buf, flags);
            }

            last = self.free_descriptor_head;
            self.free_descriptor_head = desc.next;

            // Copy to device-visible descriptor table.
            self.write_descriptor(last);
        };

        // First, assign all input (read-only) buffers.
        for input in inputs {
            assert_ne!(input.len(), 0);
            assign_buffer((*input).into(), DescriptorFlags::VIRTQ_DESC_F_NEXT);
        }

        // Then assign all output (writable) buffers.
        for output in outputs.iter_mut() {
            assert_ne!(output.len(), 0);
            assign_buffer((*output).into(), DescriptorFlags::VIRTQ_DESC_F_NEXT | DescriptorFlags::VIRTQ_DESC_F_WRITE);
        }

        // Clear the NEXT flag on the final descriptor to terminate the chain.
        self.descriptor_backup[last as usize]
            .flags
            .remove(DescriptorFlags::VIRTQ_DESC_F_NEXT);
        self.write_descriptor(last);

        // Update accounting.
        self.used_count += (inputs.len() + outputs.len()) as u16;

        head
    }


    /// Copies a single descriptor from the internal backup to the shared DMA region.
    ///
    /// This syncs the state of the descriptor at the given index into the
    /// device-visible descriptor table, ensuring that the device sees the
    /// latest address, length, flags, and next fields.
    ///
    /// # Arguments
    /// - `index`: Index of the descriptor to copy from the backup.
    fn write_descriptor(&mut self, index: u16) {
        let backup = &self.descriptor_backup[index as usize];
        let shared = unsafe { self.descriptors.as_mut() };

        // Copy updated descriptor into the device-visible region.
        shared[index as usize] = *backup;
    }


    /// Recycles a used chain of descriptors back into the freelist.
    ///
    /// This restores the descriptor chain starting at `chain_head` to the freelist,
    /// unlinking buffers and updating the backup and device-visible descriptor table.
    ///
    /// # Arguments
    /// - `chain_head`: Index of the first descriptor in the used chain.
    /// - `inputs`: Slice of input buffers (previously submitted).
    /// - `outputs`: Slice of output buffers (previously submitted).
    pub unsafe fn recycle_descriptors<'a, 'b>(
        &mut self,
        chain_head: u16,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) {
        let mut current = Some(chain_head);
        let original_head = self.free_descriptor_head;

        // Insert the recycled chain at the front of the freelist.
        self.free_descriptor_head = chain_head;

        // Closure to process each descriptor and restore it.
        let mut recycle = |buf_len: usize| {
            let index = current.expect("Descriptor chain ended unexpectedly");
            let entry = &mut self.descriptor_backup[index as usize];

            assert_ne!(buf_len, 0, "Buffer length must not be zero");

            entry.unset_buffer();
            self.used_count = self.used_count.saturating_sub(1);

            current = entry.next();

            // If we're at the end of the chain, link to the old freelist head.
            if current.is_none() {
                entry.next = original_head;
            }

            // Write updated state back to the device-visible descriptor table.
            self.write_descriptor(index);
        };

        // Recycle all input descriptors.
        for input in inputs {
            recycle(input.len());
        }

        // Recycle all output descriptors.
        for output in outputs.iter_mut() {
            recycle(output.len());
        }

        // Sanity check: we should have consumed exactly the expected number of descriptors.
        if current.is_some() {
            panic!("Descriptor chain has extra descriptors");
        }
    }

    /// Checks whether a used descriptor is available to be processed by the driver.
    ///
    /// Compares the driver's last seen used ring index with the device-updated index
    /// to determine if new buffers have been returned.
    ///
    /// # Returns
    /// - `true` if a used descriptor can be popped from the ring.
    /// - `false` if no new descriptors have been used by the device.
    pub fn can_pop(&self) -> bool {
        // Acquire ensures we see the latest writes by the device to the used ring.
        self.last_used_ring_index != unsafe {
            (*self.used.as_ptr()).index.load(Ordering::Acquire)
        }
    }


    /// Pops a used descriptor from the ring and verifies its identity.
    ///
    /// Checks whether a new used entry is available, verifies that the returned
    /// descriptor matches the expected token (head index), and recycles the chain.
    ///
    /// # Arguments
    /// - `expected_token`: The head index originally submitted to the queue.
    /// - `inputs`: The original input buffers submitted with the descriptor chain.
    /// - `outputs`: The original output buffers submitted with the descriptor chain.
    ///
    /// # Returns
    /// - `Ok(length)`: Number of bytes the device wrote to the output buffer(s).
    /// - `Err`: If the ring is empty or the returned token doesn't match.
    /// 
    /// # Safety
    /// Caller must ensure that `inputs` and `outputs` match the original buffers
    /// used in `add()`. This function assumes exclusive access to the queue state.
    pub unsafe fn pop_used<'a, 'b>(
        &mut self,
        expected_token: u16,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u32, String> {
        // Ensure there is a used descriptor available.
        if !self.can_pop() {
            return Err("Used ring is empty".into());
        }

        // Read the next used entry from the ring.
        let used_index = (self.last_used_ring_index & (SIZE as u16 - 1)) as usize;
        let used_entry = &(*self.used.as_ptr()).ring[used_index];

        let actual_token = used_entry.id as u16;
        let length_processed = used_entry.length;

        // Validate that the returned token matches the submitted one.
        if actual_token != expected_token {
            return Err(format!(
                "Mismatched token: expected {}, got {}",
                expected_token, actual_token
            ));
        }

        // Recycle the used descriptor chain back into the freelist.
        self.recycle_descriptors(actual_token, inputs, outputs);

        // Update our view of the used ring.
        self.last_used_ring_index = self.last_used_ring_index.wrapping_add(1);

        // Update the used_event field if the event index feature is active.
        if self.event_index_enabled {
            let used_event = &(*self.available.as_ptr()).used_event;
            used_event.store(self.last_used_ring_index, Ordering::Release);
        }

        Ok(length_processed)
    }



    /// Submits a descriptor chain to the device and notifies it.
    ///
    /// Wraps the process of adding input/output buffers to the queue and
    /// sending a notification to the device via the transport layer.
    ///
    /// # Arguments
    /// - `inputs`: Read-only buffers to send to the device.
    /// - `outputs`: Writable buffers for the device to fill.
    /// - `transport`: The transport used to notify the device of the new submission.
    ///
    /// # Returns
    /// - `Ok(token)`: The descriptor index (head) of the submitted chain.
    /// - `Err`: If descriptor allocation fails or the ring is full.
    pub fn submit<'a>(
        &mut self,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
        transport: &mut Transport,
    ) -> Result<u16, String> {
        // Add buffers to the queue and get the descriptor chain head.
        let token = unsafe { self.add(inputs, outputs) }?;

        // Notify the device that a new request is available.
        transport.notify(self.queue_index);

        Ok(token)
    }


    /// Receives and validates the device’s response for a previously submitted request.
    ///
    /// Checks if a response is available in the used ring, then pops it and verifies
    /// that the returned token matches the expected one.
    ///
    /// # Arguments
    /// - `token`: The descriptor chain head previously returned by `submit()`.
    /// - `inputs`: Original input buffers used in the request.
    /// - `outputs`: Original output buffers expected to be filled by the device.
    ///
    /// # Returns
    /// - `Ok(length)`: Number of bytes written by the device.
    /// - `Err`: If no response is available yet or token mismatch occurs.
    pub fn receive_answer<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> Result<u32, String> {
        if !self.can_pop() {
            return Err("No response yet".into());
        }

        unsafe { self.pop_used(token, inputs, outputs) }
    }


}

/// -----------------------------------------------------------------
/// Helper Functions
/// -----------------------------------------------------------------

/// Computes the sizes (in bytes) for the descriptor table, available ring, and used ring.
/// The queue size must be a power of two.
///
/// # Parameters
/// - `queue_size`: The number of descriptors in the queue.
///
/// # Returns
/// A tuple `(desc_size, avail_size, used_size)` representing the sizes in bytes.
fn get_queue_sizes(queue_size: u16) -> (usize, usize, usize) {
    assert!(queue_size.is_power_of_two(), "Queue size must be a power of two");

    let size = usize::from(queue_size);
    let desc_size = size * size_of::<Descriptor>();

    let num_avail_fields = 3 + size;
    let avail_size = num_avail_fields * size_of::<u16>();

    let num_used_fields = 3; // flags, index, avail_event
    let used_size = num_used_fields * size_of::<u16>() + size * size_of::<UsedRingElement>();

    (desc_size, avail_size, used_size)
}

