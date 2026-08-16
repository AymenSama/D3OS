/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: open_objects                                                    ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Managing opened objects in a global table (OPEN_OBJECTS). And providing ║
   ║ all major functions for the naming service.                             ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Michael Schoettner, Univ. Duesseldorf, 07.04.2026               ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::result::Result;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Once;
use spin::rwlock::RwLock;
use log::{info, warn};
use uuid::Uuid;

use super::lookup;
use super::traits::NamedObject;
use crate::process::core_local_storage::scheduler;
use naming::shared_types::{DirEntry, OpenOptions, SeekOrigin};
use syscall::return_vals::{Errno, SyscallResult};

/// Max. number of open objetcs
const MAX_OPEN_OBJECTS: usize = 0x1000;

//static OPEN_OBJECTS: Once<Arc<Mutex<Box<OpenObjectTable>>>> = Once::new();
static OPEN_OBJECTS: Once<Arc<OpenObjectTable>> = Once::new();

struct OpenObjectTable {
    open_handles: RwLock<Vec<(usize, Option<Arc<OpenedObject>>)>>,
    free_handles: RwLock<Box<[usize; MAX_OPEN_OBJECTS]>>,
}

pub(super) fn open_object_table_init() {
    OPEN_OBJECTS.call_once(|| Arc::new(OpenObjectTable::new()));
}

pub(super) fn open(path: &str, flags: OpenOptions) -> Result<usize, Errno> {
    info!("open_object::open: open called for path '{}', flags={:?}", path, flags);
    // try to open the named object for the given path
    let result = lookup::lookup_named_object(path);
    if result.is_err() {
        return Err(Errno::ENOENT);
    }
    let found_named_object: NamedObject = result.unwrap();

    // check if path is a directory and this was requested
    if flags.contains(OpenOptions::DIRECTORY) {
        if !found_named_object.is_dir() {
            return Err(Errno::ENOTDIR);
        }
    }

    // call the 'open' for a directory with flag `WRITEONLY`
    if found_named_object.is_dir() {
        if flags.contains(OpenOptions::WRITEONLY) || flags.contains(OpenOptions::READWRITE) {
            return Err(Errno::EISDIR);
        }
    }

    let opened_pipe = if found_named_object.is_pipe() {
        let pipe = found_named_object.as_pipe()?.clone();
        pipe.open(flags)?; // ignore return value
        Some(pipe)
    } else {
        None
    };

    // try to allocate an new handle
    let result = get_open_object_table().allocate_handle(Arc::new(OpenedObject::new(Arc::new(found_named_object), AtomicUsize::new(0), flags, current_owner())));

    if result.is_err() {
        if let Some(pipe) = opened_pipe {
            pipe.close(flags);
        }
    }

    result
}

pub(super) fn write(fh: usize, buf: &[u8]) -> Result<usize, Errno> {
    get_open_object_table().lookup_owned(fh, current_owner()).and_then(|opened_object| {
        if opened_object.named_object.is_file() {
            // Make `opened_object` mutable here
            return opened_object.named_object.as_file().and_then(|file| {
                let pos = opened_object.pos.load(Ordering::SeqCst);
                let bytes_written = file.write(buf, pos, opened_object.options)?;
                opened_object.pos.store(pos + bytes_written, Ordering::SeqCst);
                Ok(bytes_written) // Return the bytes written
            });
        }
        if opened_object.named_object.is_pipe() {
            // Make `opened_object` mutable here
            return opened_object.named_object.as_pipe().and_then(|pipe| {
                let bytes_written = pipe.write(buf, 0, opened_object.options)?;
                Ok(bytes_written) // Return the bytes written
            });
        }

        if opened_object.named_object.is_dir() {
            return Err(Errno::EBADF);
        }

        Err(Errno::ENOTSUP)
    })
}

pub(super) fn read(fh: usize, buf: &mut [u8]) -> Result<usize, Errno> {
    get_open_object_table().lookup_owned(fh, current_owner()).and_then(|opened_object| {
        if opened_object.named_object.is_file() {
            // Make `opened_object` mutable here
            return opened_object.named_object.as_file().and_then(|file| {
                let pos = opened_object.pos.load(Ordering::SeqCst);
                let bytes_read = file.read(buf, pos, opened_object.options)?;
                opened_object.pos.store(pos + bytes_read, Ordering::SeqCst);
                Ok(bytes_read) // Return the bytes read
            });
        }
        if opened_object.named_object.is_pipe() {
            // Make `opened_object` mutable here
            return opened_object.named_object.as_pipe().and_then(|pipe| {
                let bytes_read = pipe.read(buf, 0, opened_object.options)?;
                Ok(bytes_read) // Return the bytes written
            });
        }

        if opened_object.named_object.is_dir() {
            return Err(Errno::EISDIR);
        }
        
        Err(Errno::ENOTSUP)
    })
}

pub fn seek(fh: usize, offset: isize, origin: SeekOrigin) -> Result<usize, Errno> {
    get_open_object_table().lookup_owned(fh, current_owner()).and_then(|opened_object| {
        if opened_object.named_object.is_file() {
            // Make `opened_object` mutable here
            return opened_object.named_object.as_file().and_then(|file| {
                let new_pos = match origin {
                    SeekOrigin::Start => offset,
                    SeekOrigin::End => file.stat()?.size as isize + offset,
                    SeekOrigin::Current => opened_object.pos.load(Ordering::SeqCst) as isize + offset,
                } as usize;
                opened_object.pos.store(new_pos, Ordering::SeqCst);
                Ok(new_pos) // Success
            });
        }
        Err(Errno::ENOTSUP)
    })
}

pub(super) fn readdir(fh: usize) -> Result<Option<DirEntry>, Errno> {
    get_open_object_table().lookup_owned(fh, current_owner()).and_then(|opened_object| {
        if opened_object.named_object.is_dir() {
            // Make `opened_object` mutable here
            return opened_object.named_object.as_dir().and_then(|dir| {
                let pos = opened_object.pos.load(Ordering::SeqCst);
                let dir_entry = dir.readdir(pos)?;
                opened_object.pos.store(pos + 1, Ordering::SeqCst);
                Ok(dir_entry) // Return the DirEntry
            });
        }
        Err(Errno::ENOTSUP)
    })
}

pub(super) fn close(fh: usize) -> Result<usize, Errno> {
    info!("open_object::close: close called for fh={}", fh);

    // Reject foreign handles before touching the pipe or freeing the slot, so a
    // process cannot close another process's handle.
    let opened_object = get_open_object_table().lookup_owned(fh, current_owner())?;
    if opened_object.named_object.is_pipe() {
        if let Ok(pipe) = opened_object.named_object.as_pipe() {
            pipe.close(opened_object.options);
        }
    }

    get_open_object_table().free_handle(fh)
}

/// Reclaim every handle owned by `owner`, restoring pipe endpoint counts.
///
/// Two phases: `drain_owned` removes all owned entries under the table locks and
/// returns them, then pipe endpoints are closed *without* holding those locks.
/// `Pipe::close` wakes waiters via the scheduler, so holding the global table
/// write lock across it would stall `open`/`read` on other cores. Draining first
/// also makes the sweep atomic against a concurrent `close(fh)` from a dying
/// sibling: whichever side removes the entry owns the `pipe.close`, so an
/// endpoint counter cannot be decremented twice.
pub(super) fn close_all_for_process(owner: Uuid) -> usize {
    let victims = get_open_object_table().drain_owned(owner);
    for opened_object in &victims {
        if opened_object.named_object.is_pipe() {
            if let Ok(pipe) = opened_object.named_object.as_pipe() {
                pipe.close(opened_object.options);
            }
        }
    }
    let reclaimed = victims.len();
    if reclaimed > 0 {
        info!("naming: reclaimed {} handles for pid {}", reclaimed, owner);
    }
    reclaimed
}

/*pub(super) fn dump() {
    get_open_object_table().lock().dump();
}*/

/// ************************ OpenedObject ************************

impl OpenObjectTable {
    /// Create a new OpenObjectTable
    fn new() -> OpenObjectTable {
        OpenObjectTable {
            open_handles: RwLock::new(Vec::with_capacity(MAX_OPEN_OBJECTS)),
            free_handles: RwLock::new(Box::new([0; MAX_OPEN_OBJECTS])),
        }
    }

    /// Lookup an 'OpenedObject' for a given handle
    fn lookup_opened_object(&self, handle: usize) -> Result<Arc<OpenedObject>, Errno> {
        let guard = self.open_handles.read();
        guard
            .iter()
            .find(|(h, _)| *h == handle)
            .and_then(|(_, obj)| obj.as_ref())
            .cloned()
            .ok_or(Errno::EINVALH)
    }

    /// Lookup an 'OpenedObject' for a given handle, rejecting access by any
    /// process other than the one that opened it. A foreign handle is reported
    /// as `EINVALH` (the same code as a nonexistent handle) so ownership is not
    /// disclosed across processes.
    fn lookup_owned(&self, handle: usize, owner: Uuid) -> Result<Arc<OpenedObject>, Errno> {
        let opened_object = self.lookup_opened_object(handle)?;
        if opened_object.owner != owner {
            warn!("open_object: process {} tried to access foreign handle {}", owner, handle);
            return Err(Errno::EINVALH);
        }
        Ok(opened_object)
    }

    /// Allocate a new handle for a given 'OpenObject'
    fn allocate_handle(&self, opened_object: Arc<OpenedObject>) -> Result<usize, Errno> {
        // We modify both vectors, so take a write lock
        let mut guard = self.open_handles.write();

        // find a free slot
        let handle = self.find_free_handle().ok_or(Errno::ENOHANDLES)?;
        guard.push((handle, Some(opened_object)));
        Ok(handle)
    }

    /// Free handle
    fn free_handle(&self, handle: usize) -> SyscallResult {
        let mut guard = self.open_handles.write();

        if let Some(idx) = guard.iter().position(|(h, _)| *h == handle) {
            guard.swap_remove(idx);
            self.free_handles.write()[handle] = 0;
            Ok(0)
        } else {
            Err(Errno::EINVALH)
        }
    }

    /// Remove and return every entry owned by `owner`, freeing each handle slot.
    ///
    /// Pipe endpoints are *not* closed
    /// here (see `close_all_for_process`); this only touches the table.
    fn drain_owned(&self, owner: Uuid) -> Vec<Arc<OpenedObject>> {
        let mut guard = self.open_handles.write();
        let mut free = self.free_handles.write();

        let mut victims = Vec::new();
        guard.retain(|(handle, obj)| {
            let owned = obj.as_ref().is_some_and(|obj| obj.owner == owner);
            if owned {
                free[*handle] = 0;
                if let Some(obj) = obj {
                    victims.push(obj.clone());
                }
            }
            !owned
        });
        victims
    }

    /// Helper function of 'allocate' to find a free handle
    fn find_free_handle(&self) -> Option<usize> {
        let mut free = self.free_handles.write(); // hold guard for the whole loop
        for (i, v) in free.iter_mut().enumerate() {
            if *v == 0 {
                *v = 1;
                return Some(i);
            }
        }
        None
    }

    /*
    fn dump(&self) {
        info!("OpenObjectTable: dumping used handles");
        for (handle, opened_object) in &self.open_handles {
            if let Some(opened_object) = opened_object {
                info!(
                    "    handle = {:?}, named object = {:?}",
                    handle, opened_object.named_object
                );
            }
        }
    }
    */
}

/// Helper function returning safe access to the open object table (oot)
fn get_open_object_table() -> Arc<OpenObjectTable> {
    OPEN_OBJECTS.get().unwrap().clone()
}

/// ************************ OpenedObject ************************

// Opened object stored in the 'OpenObjectTable'
// (includes NamedObject, current position within object, options, and the id
//  of the process that opened it)
pub struct OpenedObject {
    named_object: Arc<NamedObject>,
    pos: AtomicUsize, // current position within file or number of next DirEntry
    options: OpenOptions,
    owner: Uuid, // id of the process that opened this object
}

impl OpenedObject {
    pub fn new(named_object: Arc<NamedObject>, pos: AtomicUsize, options: OpenOptions, owner: Uuid) -> OpenedObject {
        OpenedObject { named_object, pos, options, owner }
    }
}

/// Return the id of the process that owns the current thread.
///
/// Uses `try_current_thread` because the naming service is initialized at boot
/// before the scheduler has a current thread. `Uuid::nil()` is the kernel
/// process id (see `Process::new_kernel`), so kernel- and boot-time opens are
/// owned by the kernel process, which never exits and is never reclaimed.
fn current_owner() -> Uuid {
    scheduler()
        .try_current_thread()
        .map(|thread| thread.process().id())
        .unwrap_or_else(Uuid::nil)
}
