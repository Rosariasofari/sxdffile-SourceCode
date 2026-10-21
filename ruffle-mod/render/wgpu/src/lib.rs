// Remove this when we decide on how to handle multithreaded rendering (especially on wasm)
#![allow(clippy::arc_with_non_send_sync)]

use crate::backend::ActiveFrame;
use crate::bitmaps::BitmapSamplers;
use crate::buffer_pool::{BufferPool, PoolEntry};
use crate::mesh::BitmapBinds;
use crate::pipelines::Pipelines;
use crate::target::{RenderTarget, SwapChainTarget};
use crate::utils::{
    BufferDimensions, capture_image, create_buffer_with_data, format_list, get_backend_names,
};
use bytemuck::{Pod, Zeroable};
use descriptors::Descriptors;
use enum_map::Enum;
use ruffle_render::backend::RawTexture;
use ruffle_render::bitmap::{BitmapHandle, BitmapHandleImpl, PixelRegion, SyncHandle};
use ruffle_render::shape_utils::GradientType;
use ruffle_render::tessellator::{Gradient as TessGradient, Vertex as TessVertex};
use std::any::Any;
use std::cell::{Cell, OnceCell};
use std::sync::Arc;
use swf::GradientSpread;
pub use wgpu;
pub use wgpu_profiler;

type Error = Box<dyn std::error::Error>;

#[macro_use]
pub mod utils;

mod bitmaps;
mod context3d;
mod globals;
mod pipelines;
mod pixel_bender;
pub mod target;

pub mod backend;
mod blend;
mod buffer_builder;
mod buffer_pool;
#[cfg(feature = "clap")]
pub mod clap;
pub mod descriptors;
mod dynamic_transforms;
mod filters;
mod layouts;
mod mesh;
mod shaders;
mod surface;

impl BitmapHandleImpl for Texture {}

/// 纹理被引用（用于绘制/取样）的累计次数。
///
/// 配合帧数就能算出「每帧平均引用多少个纹理」，和存活的纹理总数一比，
/// 就能判断是否存在大量「冷纹理」—— 那才是可以安全淘汰的部分。
static TEX_REF_COUNT: AtomicUsize = AtomicUsize::new(0);

/// 累计纹理引用次数。
pub fn texture_ref_count() -> usize {
    TEX_REF_COUNT.load(Ordering::Relaxed)
}

pub fn as_texture(handle: &BitmapHandle) -> &Texture {
    TEX_REF_COUNT.fetch_add(1, Ordering::Relaxed);
    <dyn Any>::downcast_ref(&*handle.0).unwrap()
}

pub fn raw_texture_as_texture(handle: &dyn RawTexture) -> &wgpu::Texture {
    <dyn Any>::downcast_ref(handle).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum MaskState {
    NoMask,
    DrawMaskStencil,
    DrawMaskedContent,
    ClearMaskStencil,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Transforms {
    world_matrix: [[f32; 4]; 4],
    mult_color: [f32; 4],
    add_color: [f32; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct PosVertex {
    position: [f32; 2],
}

impl From<TessVertex> for PosVertex {
    fn from(vertex: TessVertex) -> Self {
        Self {
            position: [vertex.x, vertex.y],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct PosUvVertex {
    position: [f32; 2],
    uv: [f32; 3],
}

impl PosUvVertex {
    pub fn new(x: f32, y: f32, u: f32, v: f32, t: f32) -> Self {
        let position = [x, y];
        let uv = [u, v, t];
        Self { position, uv }
    }

    pub fn from_tessellator(vertex: TessVertex, texture_matrix: &[[f32; 3]; 3]) -> Self {
        let position = [vertex.x, vertex.y];
        let uv = Self::transform_uv(texture_matrix, vertex.x, vertex.y);
        Self { position, uv }
    }

    fn transform_uv(matrix: &[[f32; 3]; 3], x: f32, y: f32) -> [f32; 3] {
        [
            matrix[0][0] * x + matrix[1][0] * y + matrix[2][0],
            matrix[0][1] * x + matrix[1][1] * y + matrix[2][1],
            1.0,
        ]
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct PosColorVertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl From<TessVertex> for PosColorVertex {
    fn from(vertex: TessVertex) -> Self {
        Self {
            position: [vertex.x, vertex.y],
            color: [
                f32::from(vertex.color.r) / 255.0,
                f32::from(vertex.color.g) / 255.0,
                f32::from(vertex.color.b) / 255.0,
                f32::from(vertex.color.a) / 255.0,
            ],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct GradientUniforms {
    focal_point: f32,
    interpolation: i32,
    shape: i32,
    repeat: i32,
}

impl From<TessGradient> for GradientUniforms {
    fn from(gradient: TessGradient) -> Self {
        Self {
            focal_point: gradient.focal_point.to_f32().clamp(-0.98, 0.98),
            interpolation: (gradient.interpolation == swf::GradientInterpolation::LinearRgb) as i32,
            shape: match gradient.gradient_type {
                GradientType::Linear => 1,
                GradientType::Radial => 2,
                GradientType::Focal => 3,
            },
            repeat: match gradient.repeat_mode {
                GradientSpread::Pad => 1,
                GradientSpread::Reflect => 2,
                GradientSpread::Repeat => 3,
            },
        }
    }
}

#[derive(Debug)]
pub enum QueueSyncHandle {
    AlreadyCopied {
        index: Option<wgpu::SubmissionIndex>,
        buffer: PoolEntry<wgpu::Buffer, BufferDimensions>,
        copy_dimensions: BufferDimensions,
        descriptors: Arc<Descriptors>,
    },
    NotCopied {
        handle: BitmapHandle,
        copy_area: PixelRegion,
        descriptors: Arc<Descriptors>,
        pool: Arc<BufferPool<wgpu::Buffer, BufferDimensions>>,
    },
}

impl SyncHandle for QueueSyncHandle {}

impl QueueSyncHandle {
    pub fn capture<R, F: FnOnce(&[u8], u32) -> R>(
        self,
        with_rgba: F,
        frame: &mut ActiveFrame,
    ) -> R {
        match self {
            QueueSyncHandle::AlreadyCopied {
                index,
                buffer,
                copy_dimensions,
                descriptors,
            } => capture_image(
                &descriptors.device,
                &buffer,
                &copy_dimensions,
                index,
                with_rgba,
            ),
            QueueSyncHandle::NotCopied {
                handle,
                copy_area,
                descriptors,
                pool,
            } => {
                let texture = as_texture(&handle);

                let buffer_dimensions = BufferDimensions::new(
                    copy_area.width() as usize,
                    copy_area.height() as usize,
                    texture.texture.format(),
                );

                let buffer = pool.take(&descriptors, buffer_dimensions.clone());
                frame.command_encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: copy_area.x_min,
                            y: copy_area.y_min,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(buffer_dimensions.padded_bytes_per_row),
                            rows_per_image: None,
                        },
                    },
                    wgpu::Extent3d {
                        width: copy_area.width(),
                        height: copy_area.height(),
                        depth_or_array_layers: 1,
                    },
                );
                let index = frame.submit_direct(&descriptors);

                let image = capture_image(
                    &descriptors.device,
                    &buffer,
                    &buffer_dimensions,
                    Some(index),
                    with_rgba,
                );

                // After we've read pixels from a texture enough times, we'll store this buffer so that
                // future reads will be faster (it'll copy as part of the draw process instead)
                texture
                    .copy_count
                    .set(texture.copy_count.get().saturating_add(1));

                image
            }
        }
    }
}

// ---- SXDffle: GPU 资源字节统计 ----
//
// 真机证明内存大头在 /dev/kgsl-3d0（Adreno 显存）。要定位具体是谁，
// 必须把「位图纹理」和「Shape 网格缓冲」分开计量。
use std::sync::atomic::{AtomicUsize, Ordering};

static GPU_TEX_BYTES: AtomicUsize = AtomicUsize::new(0);
static GPU_TEX_COUNT: AtomicUsize = AtomicUsize::new(0);

/// 返回 (位图纹理总字节, 位图纹理个数)。
pub use crate::mesh::mesh_gpu_stats;

pub fn texture_gpu_stats() -> (usize, usize) {
    (
        GPU_TEX_BYTES.load(Ordering::Relaxed),
        GPU_TEX_COUNT.load(Ordering::Relaxed),
    )
}

pub(crate) fn note_texture_alloc(bytes: usize) {
    if bytes > 0 {
        GPU_TEX_BYTES.fetch_add(bytes, Ordering::Relaxed);
        GPU_TEX_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn note_texture_free(bytes: usize) {
    if bytes > 0 {
        GPU_TEX_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        GPU_TEX_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 至少被绘制过一次的纹理数量（冷热判断用）。
static TEX_USED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// 返回「曾被绘制过」的纹理数量。
pub fn texture_used_count() -> usize {
    TEX_USED_COUNT.load(Ordering::Relaxed)
}

static TEX_BUCKETS: [AtomicUsize; 6] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// 位图纹理尺寸分布：≤32 / ≤64 / ≤128 / ≤256 / ≤512 / >512（按长边）。
///
/// 用来判断显存放大是否来自「大量小纹理」—— 小纹理在 GPU 上有固定粒度的
/// 对齐与元数据开销，数量一多，实际占用会远超像素字节。
pub fn texture_size_histogram() -> [usize; 6] {
    [
        TEX_BUCKETS[0].load(Ordering::Relaxed),
        TEX_BUCKETS[1].load(Ordering::Relaxed),
        TEX_BUCKETS[2].load(Ordering::Relaxed),
        TEX_BUCKETS[3].load(Ordering::Relaxed),
        TEX_BUCKETS[4].load(Ordering::Relaxed),
        TEX_BUCKETS[5].load(Ordering::Relaxed),
    ]
}

/// 记录一张纹理的尺寸档位（创建时调用一次）。
pub(crate) fn note_texture_size(max_side: u32) {
    let idx = match max_side {
        0..=32 => 0,
        33..=64 => 1,
        65..=128 => 2,
        129..=256 => 3,
        257..=512 => 4,
        _ => 5,
    };
    TEX_BUCKETS[idx].fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug)]
pub struct Texture {
    pub(crate) texture: wgpu::Texture,
    repeating_linear: OnceCell<BitmapBinds>,
    repeating_nearest: OnceCell<BitmapBinds>,
    clamped_linear: OnceCell<BitmapBinds>,
    clamped_nearest: OnceCell<BitmapBinds>,
    copy_count: Cell<u8>,
    /// 纹理字节数（宽 × 高 × 4），用于 GPU 内存统计
    pub(crate) gpu_bytes: usize,
    /// 是否至少被绘制过一次。
    ///
    /// 只有在真正参与绘制时才会创建 bind group（且之后被 OnceCell 缓存），
    /// 所以这个标记等价于「这张纹理进过画面」。
    /// 存活但从未被画过的纹理 = 冷纹理，是唯一可以安全淘汰的部分。
    pub(crate) used: Cell<bool>,
}

impl Drop for Texture {
    fn drop(&mut self) {
        note_texture_free(self.gpu_bytes);
        // used 计数也要随纹理销毁一起回退，否则会累加成"曾被绘制 39912 / 存活 6123"
        if self.used.get() {
            TEX_USED_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Texture {
    pub fn bind_group(
        &self,
        repeating: bool,
        smoothed: bool,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        handle: BitmapHandle,
        samplers: &BitmapSamplers,
    ) -> &BitmapBinds {
        let bind = match (repeating, smoothed) {
            (true, true) => &self.repeating_linear,
            (true, false) => &self.repeating_nearest,
            (false, true) => &self.clamped_linear,
            (false, false) => &self.clamped_nearest,
        };
        bind.get_or_init(|| {
            if !self.used.get() {
                self.used.set(true);
                TEX_USED_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            BitmapBinds::new(
                device,
                layout,
                samplers.get_sampler(repeating, smoothed),
                self.texture.create_view(&Default::default()),
                create_debug_label!("Bitmap {:?} bind group (smoothed: {})", handle.0, smoothed),
            )
        })
    }
}
