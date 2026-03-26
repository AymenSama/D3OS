/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: rectangle_demo.rs                                               ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Animated rectangle demo rendered on the Virtio GPU framebuffer.         ║
   ║                                                                         ║
   ║ Responsibilities:                                                       ║
   ║   - Spawn 20+ moving colored rectangles                                 ║
   ║   - Handle bouncing and direction reversal at screen edges              ║
   ║   - Log real-time FPS and render time statistics                        ║
   ║                                                                         ║
   ║ Author: Nikita E., Univ. Duesseldorf, 2025                              ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use alloc::vec::Vec;
use core::slice;
use log::info;
use crate::device::virtio::gpu::gpu::VirtioGpu;
use crate::device::virtio::gpu::renderer::Graphics;
use crate::syscall::sys_concurrent::sys_thread_sleep;
use crate::syscall::sys_time::sys_get_system_time;

/// Represents a colored rectangle that moves and bounces around the screen.
struct MovingRect {
    /// X coordinate of the top-left corner (floating point for smooth movement).
    x: f32,
    /// Y coordinate of the top-left corner.
    y: f32,
    /// Width of the rectangle in pixels.
    width: usize,
    /// Height of the rectangle in pixels.
    height: usize,
    /// Horizontal velocity in pixels per second.
    vx: f32,
    /// Vertical velocity in pixels per second.
    vy: f32,
    /// RGBA color of the rectangle.
    color: [u8; 4],
}

/// Starts the rectangle animation demo on the Virtio GPU framebuffer.
///
/// Initializes 20 rectangles with different colors, positions, and velocities,
/// then enters a loop to update their positions, handle bouncing, and redraw.
///
/// # Parameters
/// - `gpu`: Reference to the initialized VirtioGpu instance.
pub fn rectangle_demo(gpu: &VirtioGpu) {
    // Retrieve resolution and framebuffer pointer from the GPU.
    let (fb_ptr, fb_len, screen_width, screen_height) = {
        let (w, h) = gpu.get_resolution().unwrap();
        let buf = gpu.initialize_framebuffer().unwrap();
        (buf.as_mut_ptr(), buf.len(), w as usize, h as usize)
    };
    let stride = screen_width * 4;

    // Safety: Interpret the framebuffer pointer as a mutable slice of RGBA bytes.
    let fb_slice = unsafe { slice::from_raw_parts_mut(fb_ptr, fb_len) };
    let mut gfx = Graphics::new(fb_slice, screen_width, screen_height, stride);

    // Rectangle dimensions (all rectangles share the same size).
    const RECT_W: usize = 50;
    const RECT_H: usize = 30;

    // Predefined array of 20 distinct RGBA colors.
    let colors: [[u8; 4]; 20] = [
        [255,   0,   0, 255], // Red
        [  0, 255,   0, 255], // Green
        [  0,   0, 255, 255], // Blue
        [255, 255,   0, 255], // Yellow
        [255,   0, 255, 255], // Magenta
        [  0, 255, 255, 255], // Cyan
        [255, 165,   0, 255], // Orange
        [128,   0, 128, 255], // Purple
        [255, 192, 203, 255], // Pink
        [  0, 128, 128, 255], // Teal
        [  0,   0, 128, 255], // Navy
        [128,   0,   0, 255], // Maroon
        [128, 128,   0, 255], // Olive
        [192, 192, 192, 255], // Silver
        [128, 128, 128, 255], // Gray
        [165,  42,  42, 255], // Brown
        [255, 215,   0, 255], // Gold
        [250, 128, 114, 255], // Salmon
        [ 64, 224, 208, 255], // Turquoise
        [238, 130, 238, 255], // Violet
    ];

    // Create a vector of 100 MovingRect instances with varied positions and velocities.
    let mut rects: Vec<MovingRect> = {
        let mut vec = Vec::with_capacity(500);
        for i in 0..500 {
            // Initial position: distribute by index using modulo arithmetic.
            let px = ((i * 37) % (screen_width - RECT_W)) as f32;
            let py = ((i * 53) % (screen_height - RECT_H)) as f32;

            // Base velocities scale with index for variation; doubled for faster motion.
            let base_vx = ((i % 5) as f32 + 1.0) * 100.0;
            let base_vy = (((i / 5) as f32 + 1.0) * 120.0).min(400.0);

            // Alternate direction horizontally and vertically based on index.
            let vx = if i % 2 == 0 { base_vx } else { -base_vx };
            let vy = if i % 3 == 0 { base_vy } else { -base_vy };

            // Use color cyclically from the predefined color list.
            let color = colors[i % colors.len()];

            vec.push(MovingRect {
                x: px,
                y: py,
                width: RECT_W,
                height: RECT_H,
                vx,
                vy,
                color,
            });
        }
        vec
    };


    // Frame timing config for ~60 FPS
    let frame_time_ms = 16.67;
    let delta_time = frame_time_ms / 1000.0;

    // Scientific FPS tracking
    const MAX_SAMPLES: usize = 1000;
    let mut frame_times_ms: [isize; MAX_SAMPLES] = [0; MAX_SAMPLES];
    let mut sample_index = 0;
    let mut samples_collected: usize = 0;
    let mut last_log_time = sys_get_system_time();

    loop {
        let frame_start = sys_get_system_time();

        // Clear screen to black before redrawing
        gfx.clear_screen([0, 0, 0, 255]);

        // Update each rectangle’s position and bounce off screen edges
        for rect in rects.iter_mut() {
            rect.x += rect.vx * delta_time;
            rect.y += rect.vy * delta_time;

            if rect.x <= 0.0 {
                rect.x = 0.0;
                rect.vx = rect.vx.abs();
            } else if rect.x + rect.width as f32 >= screen_width as f32 {
                rect.x = (screen_width - rect.width) as f32;
                rect.vx = -rect.vx.abs();
            }

            if rect.y <= 0.0 {
                rect.y = 0.0;
                rect.vy = rect.vy.abs();
            } else if rect.y + rect.height as f32 >= screen_height as f32 {
                rect.y = (screen_height - rect.height) as f32;
                rect.vy = -rect.vy.abs();
            }

            // Draw the updated rectangle
            gfx.fill_rect(
                rect.x as usize,
                rect.y as usize,
                rect.width,
                rect.height,
                rect.color,
            );
        }

        // Push framebuffer to display
        let render_start = sys_get_system_time();
        gpu.flush_framebuffer().unwrap();
        let render_end = sys_get_system_time();
        let render_time = render_end - render_start;

        // Save render time for performance metrics
        frame_times_ms[sample_index] = render_time;
        sample_index = (sample_index + 1) % MAX_SAMPLES;
        samples_collected = samples_collected.saturating_add(1).min(MAX_SAMPLES);

        // Every 30 seconds, log performance stats (FPS, ⌀, min, max, 95th percentile)
        let now = sys_get_system_time();
        if now - last_log_time >= 30_000 {
            let mut sorted = frame_times_ms[..samples_collected].to_vec();
            sorted.sort_unstable();

            let avg_render_time = sorted.iter().sum::<isize>() as f64 / samples_collected as f64;
            let fps = 1000.0 / avg_render_time;
            let min_render = *sorted.first().unwrap_or(&0);
            let max_render = *sorted.last().unwrap_or(&0);
            let p95_render = sorted[(samples_collected * 95 / 100).min(samples_collected - 1)];

            info!(
                "[Perf] FPS: {:.2}, ⌀: {:.2} ms, min: {} ms, max: {} ms, 95%: {} ms",
                fps, avg_render_time, min_render, max_render, p95_render
            );

            last_log_time = now;
        }

        // Sleep to maintain stable frame rate (16.67 ms)
        let frame_end = sys_get_system_time();
        let elapsed = frame_end - frame_start;
        if elapsed < frame_time_ms as isize {
            sys_thread_sleep((frame_time_ms as isize - elapsed) as usize);
        }
    }
}