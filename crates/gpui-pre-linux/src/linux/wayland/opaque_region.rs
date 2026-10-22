use gpui::{Bounds, Pixels, Size, WindowBackgroundAppearance, WindowDecorations, point};
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::{wl_compositor, wl_region, wl_surface},
};

pub(super) fn update_size_and_scale(
    bounds: &mut Bounds<Pixels>,
    scale: &mut f32,
    size: Option<Size<Pixels>>,
    new_scale: Option<f32>,
    refresh: impl FnOnce(Size<Pixels>),
) -> bool {
    if size.is_none_or(|size| size == bounds.size) && new_scale.is_none_or(|value| value == *scale)
    {
        return false;
    }
    if let Some(size) = size {
        bounds.size = size;
    }
    if let Some(new_scale) = new_scale {
        *scale = new_scale;
    }
    refresh(bounds.size);
    true
}

fn rectangle(
    surface_size: Size<Pixels>,
    appearance: WindowBackgroundAppearance,
    decorations: WindowDecorations,
) -> Option<Bounds<i32>> {
    if appearance != WindowBackgroundAppearance::Opaque || decorations != WindowDecorations::Server
    {
        return None;
    }
    // Server decorations have zero inset; client decorations may have translucent rounded corners.
    Some(Bounds {
        origin: point(0, 0),
        size: surface_size.map(|value| f32::from(value) as i32),
    })
}

pub(super) fn refresh<State>(
    surface: &wl_surface::WlSurface,
    compositor: &wl_compositor::WlCompositor,
    qh: &QueueHandle<State>,
    surface_size: Size<Pixels>,
    appearance: WindowBackgroundAppearance,
    decorations: WindowDecorations,
) where
    State: Dispatch<wl_region::WlRegion, ()> + 'static,
{
    let Some(area) = rectangle(surface_size, appearance, decorations) else {
        surface.set_opaque_region(None);
        return;
    };
    let region = compositor.create_region(qh, ());
    region.add(
        area.origin.x,
        area.origin.y,
        area.size.width,
        area.size.height,
    );
    surface.set_opaque_region(Some(&region));
    region.destroy();
}

#[cfg(test)]
mod tests;
