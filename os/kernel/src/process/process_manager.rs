/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: process manager                                                 ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Functions related to process management.                                ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Fabian Ruhland, Univ. Duesseldorf, 20.07.2025                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::info;
use uuid::Uuid;
use x86_64::structures::paging::frame::PhysFrameRange;
use x86_64::structures::paging::Page;
use x86_64::VirtAddr;

use crate::memory::{vmm, MemorySpace};
use crate::memory::vma::VmaType;
use crate::process::core_local_storage::scheduler;
use crate::process::process::Process;
use crate::process::process_stats::ProcStat;

pub struct ProcessManager {
    active_processes: Vec<Arc<Process>>,
    /// Processes that have exited but whose last thread may still be in
    /// `scheduler.exit()` / `Thread::switch`. Dropped on the *next* cleanup pass.
    exited_processes: Vec<Arc<Process>>,
    /// Staged from `exited_processes` last cleanup cycle; safe to drop now.
    graveyard: Vec<Arc<Process>>,
}

impl ProcessManager {
    pub const fn new() -> Self {
        Self {
            active_processes: Vec::new(),
            exited_processes: Vec::new(),
            graveyard: Vec::new(),
        }
    }

    /// Create a new process
    pub fn create_process(&mut self, name: String) -> Arc<Process> {
        let kernel_process = self.kernel_process().expect("No kernel process found!");
        let paging = vmm::clone_address_space(&(kernel_process.virtual_address_space));
        let process = Arc::new(Process::new(paging, name));
        self.active_processes.push(Arc::clone(&process));
        process
    }

    /// Create the kernel process
    pub fn create_kernel_process(&mut self, kernel_image_region: PhysFrameRange, heap_region: PhysFrameRange) -> Arc<Process> {
        let kernel_process = self.kernel_process();
        if kernel_process.is_some() {
            panic!("Kernel process already exists!");
        }

        let paging = vmm::create_kernel_address_space();
        let kernel_process = Arc::new(Process::new_kernel(paging));
        self.active_processes.push(Arc::clone(&kernel_process));

        // TODO: adjust this when removing 1:1 mapping
        kernel_process
            .virtual_address_space
            .alloc_vma(
                Some(Page::from_start_address(VirtAddr::new(heap_region.start.start_address().as_u64())).unwrap()),
                heap_region.len(),
                MemorySpace::Kernel,
                VmaType::Heap,
                "heap",
            )
            .expect("failed to create VMA for kernel heap");

        // TODO: stack is part of BSS, which is part of code
        kernel_process
            .virtual_address_space
            .alloc_vma(
                Some(Page::from_start_address(VirtAddr::new(kernel_image_region.start.start_address().as_u64())).unwrap()),
                kernel_image_region.len(),
                MemorySpace::Kernel,
                VmaType::Code,
                "code",
            )
            .expect("failed to create VMA for kernel code");
        kernel_process.dump();

        info!("Kernel process [{}]: created", kernel_process.id());

        kernel_process
    }

    /// Return the ids of all active processes
    pub fn active_process_ids(&self) -> Vec<Uuid> {
        self.active_processes.iter().map(|process| process.id()).collect()
    }

    /// Return true if the process is active
    pub fn is_active_process(&self, pid: Uuid) -> bool {
        self.active_processes.iter().any(|process| process.id() == pid)
    }

    /// Return a snapshot of the process
    pub fn get_stats(&self, pid: Uuid) -> Option<Arc<ProcStat>> {
        self.active_processes
            .iter()
            .find(|process| process.id() == pid)
            .map(|process| Arc::new(ProcStat::from_process(process)))
    }

    /// Return Vec of all Process-Stats
    pub fn get_all_stats(&self) -> Vec<Arc<ProcStat>> {
        self.active_processes
            .iter()
            .map(|process| Arc::new(ProcStat::from_process(process)))
            .collect()
    }

    /// Get reference to kernel process
    pub fn kernel_process(&self) -> Option<Arc<Process>> {
        self.active_processes.first().map(Arc::clone)
    }

    /// Get reference to current process
    pub fn current_process(&self) -> Arc<Process> {
        if self.active_processes.len() > 1 {
            scheduler().current_thread().process()
        } else {
            self.kernel_process().unwrap()
        }
    }

    /// Exit a process by its id
    pub fn exit(&mut self, process_id: Uuid) {
        let index = self
            .active_processes
            .iter()
            .position(|process| process.id() == process_id)
            .expect("Process: Trying to exit a non-existent process!");

        let process = Arc::clone(&self.active_processes[index]);
        process.kill_all_threads_but_current();

        self.active_processes.swap_remove(index);
        self.exited_processes.push(process);

        // After sibling threads are gone, before this thread exits: Pipe::close
        // needs a live caller, and nothing should open handles behind the sweep.
        crate::naming::api::close_handles_for_process(process_id);
    }

    /// Kill a process by its id
    pub fn kill(&mut self, process_id: Uuid) {
        let index = self
            .active_processes
            .iter()
            .position(|process| process.id() == process_id)
            .expect("Process: Trying to kill a non-existent process!");

        let process = Arc::clone(&self.active_processes[index]);
        for thread_id in process.thread_ids() {
            scheduler().kill(thread_id);
        }

        self.active_processes.swap_remove(index);
        self.exited_processes.push(process);

        // See `exit` for why this is last.
        crate::naming::api::close_handles_for_process(process_id);
    }

    /// Drop processes that exited at least one cleanup cycle ago.
    ///
    /// The last thread of a process cannot unmap its own address space: its
    /// kernel stack lives in that space, and `Thread::switch` still runs there
    /// after `ProcessManager::exit` returns. If cleanup was blocked on this
    /// write lock during exit, a one-cycle delay keeps the `Arc<Process>` alive
    /// across that switch. Dropping immediately is a triple-fault/reset.
    pub fn drop_exited_process(&mut self) {
        self.graveyard.clear();
        core::mem::swap(&mut self.graveyard, &mut self.exited_processes);
    }

    /// Dump all active processes
    pub fn dump(&self) {
        info!("=== Active Processes Dump ===");
        if self.active_processes.is_empty() {
            info!("   No active processes.");
            return;
        }

        for (i, process) in self.active_processes.iter().enumerate() {
            info!("Process #{}: PID={}, name={}", i, process.id(), process.name());
            process.virtual_address_space.dump(process.id());
        }
        info!("=============================");
    }
}
