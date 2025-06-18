/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: mmio_registers.rs                                               ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Defines bitfield-accessible MMIO register layouts for VirtIO PCI        ║
   ║ transport devices using the `tock-registers` crate.                     ║
   ║                                                                         ║
   ║ Contains:                                                               ║
   ║ - 32-bit registers: device/driver feature selection and negotiation     ║
   ║ - 16-bit registers: queue config, MSI-X vectors, notify offsets         ║
   ║ - 8-bit registers: device status and config generation                  ║
   ║ - 64-bit registers: queue descriptor/device/driver addresses            ║
   ║                                                                         ║
   ║ These definitions enable safe and ergonomic access to memory-mapped     ║
   ║ configuration structures of modern VirtIO PCI devices.                  ║
   ║                                                                         ║
   ║ References:                                                             ║
   ║   • Virtio Specification 1.3                                            ║
   ║     https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html     ║
   ║   • tock-registers crate                                                ║
   ║     https://crates.io/crates/tock-registers                             ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use tock_registers::register_bitfields;

// 32-bit register bitfields
register_bitfields![u32,
    pub DEVICE_FEATURE_SELECT [
        VALUE OFFSET(0) NUMBITS(32) []
    ],
    pub DEVICE_FEATURE [
        VALUE OFFSET(0) NUMBITS(32) []
    ],
    pub DRIVER_FEATURE_SELECT [
        VALUE OFFSET(0) NUMBITS(32) []
    ],
    pub DRIVER_FEATURE [
        VALUE OFFSET(0) NUMBITS(32) []
    ]
];

// 16-bit register bitfields (le16 fields)
register_bitfields![u16,
    pub CONFIG_MSIX_VECTOR [
        VALUE OFFSET(0) NUMBITS(16) []
    ],
    pub NUM_QUEUES [
        VALUE OFFSET(0) NUMBITS(16) []
    ],
    pub QUEUE_SELECT [
        VALUE OFFSET(0) NUMBITS(16) []
    ],
    pub QUEUE_SIZE [
        VALUE OFFSET(0) NUMBITS(16) []
    ],
    pub QUEUE_MSIX_VECTOR [
        VALUE OFFSET(0) NUMBITS(16) []
    ],
    pub QUEUE_ENABLE [
        VALUE OFFSET(0) NUMBITS(16) []
    ],
    pub QUEUE_NOTIFY_OFF [
        VALUE OFFSET(0) NUMBITS(16) []
    ]
    /*pub QUEUE_NOTIFY_DATA [
        VALUE OFFSET(0) NUMBITS(16) []
    ]
    pub QUEUE_RESET [
        VALUE OFFSET(0) NUMBITS(16) []
    ]*/
];

// 8-bit register bitfields
register_bitfields![u8,
    pub DEVICE_STATUS [
        VALUE OFFSET(0) NUMBITS(8) []
    ],
    pub CONFIG_GENERATION [
        VALUE OFFSET(0) NUMBITS(8) []
    ]
];

// 64-bit register bitfields for queue addresses
register_bitfields![u64,
    pub QUEUE_DESC [
        VALUE OFFSET(0) NUMBITS(64) []
    ],
    pub QUEUE_DRIVER [
        VALUE OFFSET(0) NUMBITS(64) []
    ],
    pub QUEUE_DEVICE [
        VALUE OFFSET(0) NUMBITS(64) []
    ]
];
