mod cosmic_text_system;
#[cfg(not(target_family = "wasm"))]
mod gpu_fence;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod fontconfig_on_demand;
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;

pub use cosmic_text_system::*;
#[cfg(not(target_family = "wasm"))]
pub use gpu_fence::{FenceClock, GpuCompletionSink};
pub use wgpu;
pub use wgpu_atlas::*;
pub use wgpu_context::*;
#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
pub use wgpu_renderer::WgpuHeadlessRenderer;
pub use wgpu_renderer::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
