/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: protocol.rs                                                     ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Protocol definitions for Virtio GPU command and response types.         ║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Define control headers and opcodes (2D, 3D, cursor)                 ║
   ║   - Define structs for commands and responses                           ║
   ║   - Provide Pod-safe types for DMA transfers                            ║
   ║   - Enable zero-copy interaction with the device                        ║
   ║                                                                         ║
   ║ Reference:                                                              ║
   ║   • Virtio Specification 1.3                                            ║
   ║     https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html     ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::format;
use alloc::string::{String};
use bytemuck::{Pod, Zeroable};

/// Virtio queue indices for GPU device operations.
pub const CONTROL_QUEUE: u16 = 0;
pub const CURSOR_QUEUE: u16 = 1;

/// Primary scanout identifier.
pub const SCANOUT_ID: u32 = 0;

/// Maximum number of scanout heads the device may report.
pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

/// Flag to request a fence on commands (Virtio 1.3 §5.2.1).
pub const GPU_FLAG_FENCE: u32 = 1 << 0;

//------------------------------------------------------------------------------

/// Wrapper for GPU command and response type codes (4-byte LE). §5.2.2.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct VirtioGpuCtrlType(pub u32);

impl VirtioGpuCtrlType {
    // 2D Command opcodes §5.2.3
    pub const GET_DISPLAY_INFO: Self            = Self(0x0100);
    pub const RESOURCE_CREATE2D: Self           = Self(0x0101);
    pub const RESOURCE_UNREF: Self              = Self(0x0102);
    pub const SET_SCANOUT: Self                 = Self(0x0103);
    pub const RESOURCE_FLUSH: Self              = Self(0x0104);
    pub const TRANSFER_TO_HOST2D: Self          = Self(0x0105);
    pub const RESOURCE_ATTACH_BACKING: Self     = Self(0x0106);
    pub const RESOURCE_DETACH_BACKING: Self     = Self(0x0107);
    pub const GET_CAPSET_INFO: Self             = Self(0x0108);
    pub const GET_CAPSET: Self                  = Self(0x0109);
    pub const GET_EDID: Self                    = Self(0x010a);
    pub const RESOURCE_ASSIGN_UUID: Self        = Self(0x010b);
    pub const RESOURCE_CREATE_BLOB: Self        = Self(0x010c);
    pub const SET_SCANOUT_BLOB: Self            = Self(0x010d);

    // 3D Command opcodes §5.3
    pub const CTX_CREATE: Self                  = Self(0x0200);
    pub const CTX_DESTROY: Self                 = Self(0x0201);
    pub const CTX_ATTACH_RESOURCE: Self         = Self(0x0202);
    pub const CTX_DETACH_RESOURCE: Self         = Self(0x0203);
    pub const RESOURCE_CREATE3D: Self           = Self(0x0204);
    pub const TRANSFER_TO_HOST3D: Self          = Self(0x0205);
    pub const TRANSFER_FROM_HOST3D: Self        = Self(0x0206);
    pub const SUBMIT3D: Self                    = Self(0x0207);
    pub const RESOURCE_MAP_BLOB: Self           = Self(0x0208);
    pub const RESOURCE_UNMAP_BLOB: Self         = Self(0x0209);

    // Cursor Command opcodes §5.4
    pub const UPDATE_CURSOR: Self               = Self(0x0300);
    pub const MOVE_CURSOR: Self                 = Self(0x0301);

    // Response codes
    pub const RESP_OK_NODATA: Self              = Self(0x1100);
    pub const RESP_OK_DISPLAY_INFO: Self        = Self(0x1101);
    pub const RESP_OK_CAPSET_INFO: Self         = Self(0x1102);
    pub const RESP_OK_CAPSET: Self              = Self(0x1103);
    pub const RESP_OK_EDID: Self                = Self(0x1104);
    pub const RESP_OK_RESOURCE_UUID: Self       = Self(0x1105);
    pub const RESP_OK_MAP_INFO: Self            = Self(0x1106);

    // Error responses
    pub const RESP_ERR_UNSPEC: Self             = Self(0x1200);
    pub const RESP_ERR_OUT_OF_MEMORY: Self      = Self(0x1201);
    pub const RESP_ERR_INVALID_SCANOUT_ID: Self = Self(0x1202);
    pub const RESP_ERR_INVALID_RESOURCE_ID: Self= Self(0x1203);
    pub const RESP_ERR_INVALID_CONTEXT_ID: Self = Self(0x1204);
    pub const RESP_ERR_INVALID_PARAMETER: Self  = Self(0x1205);
}

//------------------------------------------------------------------------------

/// Standard header for all control commands and responses (Virtio 1.3 §5.2.2).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuCtrlHdr {
    /// Command or response type code.
    pub type_:    u32,
    /// Flags (e.g., fence request).
    pub flags:    u32,
    /// Optional fence identifier for synchronization.
    pub fence_id: u64,
    /// 3D context identifier (unused for 2D commands).
    pub ctx_id:   u32,
    /// Reserved ring index for multi-queue support.
    pub ring_idx: u8,
    pub padding:  [u8; 3],
}

impl VirtioGpuCtrlHdr {
    /// Create a new control header with given command type.
    pub fn with_ctrl_type(t: VirtioGpuCtrlType) -> Self {
        Self { type_: t.0, flags: 0, fence_id: 0, ctx_id: 0, ring_idx: 0, padding: [0;3] }
    }

    /// Verify that a response header matches the expected type.
    pub fn check_ctrl_type(&self, expected: VirtioGpuCtrlType) -> Result<(), String> {
        if self.type_ == expected.0 {
            Ok(())
        } else {
            Err(format!("Unexpected response: got {:#x}, expected {:#x}", self.type_, expected.0))
        }
    }
}

//------------------------------------------------------------------------------

/// 2D rectangle definition (x, y, width, height) for blits and scanout. §5.2.3.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
pub struct VirtioGpuRect {
    pub x:      u32,
    pub y:      u32,
    pub width:  u32,
    pub height: u32,
}

/// Describes a scanout mode and state for one head. §5.2.3.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuScanout {
    /// Rectangle of the mode.
    pub rect:    VirtioGpuRect,
    /// 1 if enabled, 0 otherwise.
    pub enabled: u32,
    /// Reserved flags.
    pub flags:   u32,
}

/// Response structure for GET_DISPLAY_INFO containing up to 16 modes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuRespDisplayInfo {
    pub hdr:    VirtioGpuCtrlHdr,
    pub pmodes: [VirtioGpuScanout; VIRTIO_GPU_MAX_SCANOUTS],
}

//------------------------------------------------------------------------------

/// Supported pixel formats (subset). §5.2.3.
pub const B8G8R8A8UNORM: u32 = 1;

/// Command to create a 2D resource with given format and size. §5.2.4.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceCreate2d {
    pub hdr:         VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub format:      u32,
    pub width:       u32,
    pub height:      u32,
}

/// Command to set a scanout head to display a region of a resource. §5.2.6.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuSetScanout {
    pub hdr:         VirtioGpuCtrlHdr,
    pub r:           VirtioGpuRect,
    pub scanout_id:  u32,
    pub resource_id: u32,
}

/// Command to flush a rectangular region of a resource to the scanout. §5.2.8.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceFlush {
    pub hdr:         VirtioGpuCtrlHdr,
    pub r:           VirtioGpuRect,
    pub resource_id: u32,
    pub _padding:    u32,
}

/// Command to transfer resource data back to host memory for display. §5.2.7.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuTransferToHost2d {
    pub hdr:         VirtioGpuCtrlHdr,
    pub r:           VirtioGpuRect,
    pub offset:      u64,
    pub resource_id: u32,
    pub _padding:    u32,
}

/// Command to attach host memory pages as backing for a resource.
/// See Virtio 1.3 §5.2.5: a flattened single-entry version of attach and mem_entry.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct VirtioGpuResourceAttachBacking {
    pub hdr:         VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub nr_entries:  u32,
    pub addr:        u64,
    pub len:         u32,
    pub _padding:    u32,
}
