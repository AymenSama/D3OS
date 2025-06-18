/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: gpu/mod.rs                                                      ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Top-level module for the Virtio GPU driver.                             ║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Defines and initializes the global GPU driver instance              ║
   ║   - Starts a demo to verify driver functionality                        ║
   ║   - Provides accessor to the `VirtioGpu` instance                       ║
   ║                                                                         ║
   ║ Related: Virtio 1.3 Specification, §3.1 – Device Initialization         ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::sync::Arc;
use log::info;
use spin::Once;

use crate::device::virtio::gpu::gpu::VirtioGpu;
use crate::device::virtio::gpu::pong_demo::pong_demo;
use crate::device::virtio::gpu::rectangle_demo::rectangle_demo;

pub mod gpu;
mod interrupt;
pub mod protocol;
mod renderer;
mod pong_demo;
mod rectangle_demo;

/// Global singleton instance of the Virtio GPU driver.
static GPU: Once<Arc<VirtioGpu>> = Once::new();

/// Initializes the Virtio GPU subsystem.
///
/// - Constructs and activates the `VirtioGpu` driver.
/// - Queries and logs the display resolution.
/// - Launches a graphics demo to verify framebuffer output.
///
/// # Panics
/// Panics if driver initialization or resolution query fails.
pub fn init() {
    GPU.call_once(|| {
        let gpu = Arc::new(VirtioGpu::new().expect("Failed to initialize GPU driver"));
        VirtioGpu::plugin(Arc::clone(&gpu));

        let (width, height) = gpu.get_resolution().expect("Failed to get GPU resolution");
        info!("GPU resolution: {}x{}", width, height);

        //rectangle_demo(&gpu);
        //pong_demo(&gpu); // Uncomment to run Pong demo instead

        gpu
    });
}

/// Returns a clone of the global `VirtioGpu` instance, if initialized.
pub fn virtio_gpu() -> Option<Arc<VirtioGpu>> {
    GPU.get().cloned()
}
