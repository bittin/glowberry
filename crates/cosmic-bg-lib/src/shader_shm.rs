// SPDX-License-Identifier: MPL-2.0

//! Shader renderer that produces SHM buffers for Wayland subsurfaces.
//!
//! This module renders shaders to offscreen textures and produces buffers
//! suitable for use with iced's subsurface widget via shared memory (memfd).

use crate::fragment_canvas::FragmentCanvas;
use crate::gpu::GpuRenderer;
use cosmic_bg_config::ShaderSource;
use rustix::io::Errno;
use rustix::shm::ShmOFlags;
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A shader frame rendered to shared memory.
///
/// This can be used to create an SHM buffer for Wayland subsurfaces.
pub struct ShaderShmBuffer {
    /// The file descriptor for the shared memory.
    pub fd: OwnedFd,
    /// Width of the buffer in pixels.
    pub width: i32,
    /// Height of the buffer in pixels.
    pub height: i32,
    /// Stride (bytes per row).
    pub stride: i32,
}

/// A shader renderer that produces SHM buffers for subsurfaces.
///
/// This renders shader backgrounds to offscreen textures and provides
/// the pixel data in shared memory for use with Wayland subsurfaces.
pub struct ShaderShmRenderer {
    buffer_rx: mpsc::Receiver<ShaderShmBuffer>,
    stop_tx: mpsc::Sender<()>,
    _thread: JoinHandle<()>,
}

impl ShaderShmRenderer {
    /// Create a new SHM shader renderer.
    ///
    /// Returns `None` if shader initialization fails.
    pub fn new(shader_source: &ShaderSource, width: u32, height: u32) -> Option<Self> {
        let (buffer_tx, buffer_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();

        let shader_source = shader_source.clone();
        let thread = thread::spawn(move || {
            if let Err(e) = shader_shm_render_loop(shader_source, width, height, buffer_tx, stop_rx)
            {
                tracing::error!("Shader SHM renderer error: {}", e);
            }
        });

        Some(Self {
            buffer_rx,
            stop_tx,
            _thread: thread,
        })
    }

    /// Try to receive a rendered buffer without blocking.
    /// Returns the most recent buffer available, discarding older ones.
    pub fn try_recv_buffer(&self) -> Option<ShaderShmBuffer> {
        let mut latest_buffer = None;
        // Drain all available buffers and keep only the latest
        while let Ok(buffer) = self.buffer_rx.try_recv() {
            latest_buffer = Some(buffer);
        }
        latest_buffer
    }
}

impl Drop for ShaderShmRenderer {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
    }
}

/// Create a memory file descriptor for shared memory.
fn create_memfile(size: usize) -> Result<OwnedFd, Errno> {
    loop {
        let flags = ShmOFlags::CREATE | ShmOFlags::EXCL | ShmOFlags::RDWR;

        let time = SystemTime::now();
        let name = format!(
            "/cosmic-bg-shader-{}",
            time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
        );

        match rustix::io::retry_on_intr(|| rustix::shm::shm_open(&name, flags, 0o600.into())) {
            Ok(fd) => {
                // Unlink immediately so it's anonymous
                let _ = rustix::shm::shm_unlink(&name);
                // Set the size
                rustix::fs::ftruncate(&fd, size as u64)?;
                return Ok(fd);
            }
            Err(Errno::EXIST) => {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Render loop that produces SHM buffers.
fn shader_shm_render_loop(
    shader_source: ShaderSource,
    width: u32,
    height: u32,
    buffer_tx: mpsc::Sender<ShaderShmBuffer>,
    stop_rx: mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize GPU renderer
    let gpu = GpuRenderer::new();

    // Create offscreen texture and canvas
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let canvas = FragmentCanvas::new(&gpu, &shader_source, format)?;

    let texture = create_offscreen_texture(gpu.device(), width, height, format);
    let read_buffer = create_read_buffer(gpu.device(), width, height);

    canvas.update_resolution(gpu.queue(), width, height);

    // Target ~30 FPS
    let frame_duration = Duration::from_millis(33);
    let mut last_frame = Instant::now();

    // Calculate buffer sizes
    let bytes_per_pixel = 4u32;
    let stride = width * bytes_per_pixel;
    let aligned_stride = aligned_bytes_per_row(width);
    let buffer_size = (stride * height) as usize;

    loop {
        // Check for stop signal
        if stop_rx.try_recv().is_ok() {
            break;
        }

        // Frame rate limiting
        let elapsed = last_frame.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
        last_frame = Instant::now();

        // Render to texture
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        canvas.render(&gpu, &view);

        // Copy texture to read buffer
        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cosmic-bg: shm readback encoder"),
            });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &read_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(aligned_stride),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        gpu.queue().submit(std::iter::once(encoder.finish()));

        // Map and read the buffer
        let buffer_slice = read_buffer.slice(..);

        let (map_tx, map_rx) = mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = map_tx.send(result);
        });

        // Poll until mapping completes
        let _ = gpu.device().poll(wgpu::PollType::Wait);
        if map_rx.recv().is_err() {
            continue;
        }

        // Create memfd and write pixel data
        let memfd = match create_memfile(buffer_size) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::error!("Failed to create memfile: {}", e);
                read_buffer.unmap();
                continue;
            }
        };

        // Map the memfd for writing
        let data = buffer_slice.get_mapped_range();

        // Write pixel data to memfd, handling stride alignment
        // RGBA -> XRGB (or we can keep RGBA if compositor supports it)
        // For now, write as ARGB (Wayland's Xrgb8888 is actually BGRX in memory on little-endian)
        let mut pixels = Vec::with_capacity(buffer_size);
        for row in 0..height {
            let start = (row * aligned_stride) as usize;
            let end = start + stride as usize;
            // Convert RGBA to BGRA (for wl_shm ARGB8888 format on little-endian)
            for chunk in data[start..end].chunks(4) {
                if chunk.len() == 4 {
                    pixels.push(chunk[2]); // B
                    pixels.push(chunk[1]); // G
                    pixels.push(chunk[0]); // R
                    pixels.push(chunk[3]); // A
                }
            }
        }

        drop(data);
        read_buffer.unmap();

        // Write to memfd
        if let Err(e) = rustix::io::write(&memfd, &pixels) {
            tracing::error!("Failed to write to memfile: {}", e);
            continue;
        }

        // Send buffer
        let shm_buffer = ShaderShmBuffer {
            fd: memfd,
            width: width as i32,
            height: height as i32,
            stride: stride as i32,
        };

        if buffer_tx.send(shm_buffer).is_err() {
            // Receiver dropped, exit
            break;
        }
    }

    Ok(())
}

/// Create an offscreen texture for rendering.
fn create_offscreen_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cosmic-bg: shm offscreen texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Create a buffer for reading back pixel data.
fn create_read_buffer(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Buffer {
    let bytes_per_row = aligned_bytes_per_row(width);
    let buffer_size = (bytes_per_row * height) as u64;

    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cosmic-bg: shm readback buffer"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    })
}

/// Calculate the aligned bytes per row for wgpu.
fn aligned_bytes_per_row(width: u32) -> u32 {
    let bytes_per_pixel = 4u32; // RGBA
    let unpadded = width * bytes_per_pixel;
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    ((unpadded + alignment - 1) / alignment) * alignment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_bytes_per_row_alignment() {
        // Width 1 should be padded to alignment
        let aligned = aligned_bytes_per_row(1);
        assert_eq!(aligned, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    }

    #[test]
    fn create_memfile_works() {
        let fd = create_memfile(1024);
        assert!(fd.is_ok());
    }
}
