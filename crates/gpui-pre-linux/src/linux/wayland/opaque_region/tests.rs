use super::*;
use gpui::{px, size};
use std::{io::Read, os::unix::net::UnixStream};
use wayland_client::{Connection, EventQueue, Proxy, delegate_noop, protocol::wl_registry};

struct Harness;
delegate_noop!(Harness: ignore wl_registry::WlRegistry);
delegate_noop!(Harness: ignore wl_compositor::WlCompositor);
delegate_noop!(Harness: ignore wl_surface::WlSurface);
delegate_noop!(Harness: ignore wl_region::WlRegion);

struct Wire {
    connection: Connection,
    queue: EventQueue<Harness>,
    server: UnixStream,
    compositor: wl_compositor::WlCompositor,
    surface: wl_surface::WlSurface,
}

impl Wire {
    fn new() -> Self {
        let (client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue = connection.new_event_queue::<Harness>();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let compositor: wl_compositor::WlCompositor = registry.bind(1, 4, &qh, ());
        let surface = compositor.create_surface(&qh, ());
        let mut wire = Self {
            connection,
            queue,
            server,
            compositor,
            surface,
        };
        wire.requests();
        wire
    }

    fn requests(&mut self) -> Vec<(u32, u16, Vec<u32>)> {
        self.connection.flush().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match self.server.read(&mut buffer) {
                Ok(0) => panic!("Wayland connection closed"),
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("Wayland request read: {error}"),
            }
        }
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|word| u32::from_ne_bytes(word.try_into().unwrap()))
            .collect();
        assert_eq!(words.len() * 4, bytes.len());
        let mut requests = Vec::new();
        let mut offset = 0;
        while offset < words.len() {
            let length = (words[offset + 1] >> 16) as usize / 4;
            assert!(length >= 2 && offset + length <= words.len());
            requests.push((
                words[offset],
                words[offset + 1] as u16,
                words[offset + 2..offset + length].to_vec(),
            ));
            offset += length;
        }
        requests
    }

    fn refresh(
        &mut self,
        surface_size: Size<Pixels>,
        appearance: WindowBackgroundAppearance,
        decorations: WindowDecorations,
    ) {
        refresh(
            &self.surface,
            &self.compositor,
            &self.queue.handle(),
            surface_size,
            appearance,
            decorations,
        );
    }

    fn assert_opaque(&mut self, width: u32, height: u32) {
        let requests = self.requests();
        assert_eq!(requests.len(), 4, "{requests:?}");
        let region = requests[0].2[0];
        assert_eq!(
            requests[0],
            (self.compositor.id().protocol_id(), 1, vec![region])
        );
        assert_eq!(requests[1], (region, 1, vec![0, 0, width, height]));
        assert_eq!(
            requests[2],
            (self.surface.id().protocol_id(), 4, vec![region])
        );
        assert_eq!(requests[3], (region, 0, vec![]));
    }

    fn assert_clear(&mut self) {
        assert_eq!(
            self.requests(),
            vec![(self.surface.id().protocol_id(), 4, vec![0])]
        );
    }
}

#[test]
fn opaque_region_follows_tiled_fullscreen_maximized_and_restore_sizes() {
    let mut wire = Wire::new();
    for (width, height) in [
        (900, 600),
        (2536, 1386),
        (2560, 1440),
        (2540, 1400),
        (900, 600),
    ] {
        wire.refresh(
            size(px(width as f32), px(height as f32)),
            WindowBackgroundAppearance::Opaque,
            WindowDecorations::Server,
        );
        wire.assert_opaque(width, height);
    }
}

#[test]
fn opaque_region_uses_surface_coordinates_with_fractional_buffer_scale() {
    let mut wire = Wire::new();
    let logical = size(px(901.75), px(601.5));
    assert_eq!(
        logical.to_device_pixels(1.25),
        size(gpui::DevicePixels(1127), gpui::DevicePixels(752))
    );
    for _scale in [1.0, 1.25, 1.5, 2.0] {
        wire.refresh(
            logical,
            WindowBackgroundAppearance::Opaque,
            WindowDecorations::Server,
        );
        wire.assert_opaque(901, 601);
    }
}

#[test]
fn opaque_region_never_promises_translucent_or_blurred_pixels() {
    let mut wire = Wire::new();
    for decorations in [WindowDecorations::Server, WindowDecorations::Client] {
        for appearance in [
            WindowBackgroundAppearance::Transparent,
            WindowBackgroundAppearance::Blurred,
        ] {
            wire.refresh(size(px(900.0), px(600.0)), appearance, decorations);
            wire.assert_clear();
        }
    }
}

#[test]
fn opaque_region_clears_for_client_decorations_with_nonzero_rounded_inset() {
    let mut wire = Wire::new();
    let outer = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(900.0), px(600.0)),
    };
    let inner = outer.inset(px(24.5));
    assert_eq!(outer.origin, point(px(0.0), px(0.0)));
    assert_eq!(inner.origin, point(px(24.5), px(24.5)));
    assert_eq!(inner.size, size(px(851.0), px(551.0)));
    wire.refresh(
        outer.size,
        WindowBackgroundAppearance::Opaque,
        WindowDecorations::Server,
    );
    wire.assert_opaque(900, 600);
    for logical in [outer.size, inner.size] {
        wire.refresh(
            logical,
            WindowBackgroundAppearance::Opaque,
            WindowDecorations::Client,
        );
        wire.assert_clear();
    }
    wire.refresh(
        outer.size,
        WindowBackgroundAppearance::Opaque,
        WindowDecorations::Server,
    );
    wire.assert_opaque(900, 600);
}

#[test]
fn opaque_region_resize_uses_current_surface_not_fullscreen_restore_bounds() {
    let mut wire = Wire::new();
    let restore = Bounds {
        origin: point(px(80.0), px(40.0)),
        size: size(px(900.0), px(600.0)),
    };
    let mut bounds = restore;
    let mut scale = 1.0;
    for logical in [
        size(px(2536.0), px(1386.0)),
        size(px(2560.0), px(1440.0)),
        restore.size,
    ] {
        assert!(update_size_and_scale(
            &mut bounds,
            &mut scale,
            Some(logical),
            None,
            |current| {
                wire.refresh(
                    current,
                    WindowBackgroundAppearance::Opaque,
                    WindowDecorations::Server,
                );
            }
        ));
        wire.assert_opaque(
            f32::from(logical.width) as u32,
            f32::from(logical.height) as u32,
        );
        assert_eq!(bounds.size, logical);
        assert_eq!(bounds.origin, restore.origin);
        assert_eq!(restore.size, size(px(900.0), px(600.0)));
    }
}

#[test]
fn opaque_region_duplicate_resize_and_scale_declare_nothing() {
    let mut wire = Wire::new();
    let logical = size(px(900.0), px(600.0));
    let mut bounds = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: logical,
    };
    let mut scale = 1.25;
    for (size, new_scale) in [
        (None, None),
        (Some(logical), None),
        (None, Some(1.25)),
        (Some(logical), Some(1.25)),
    ] {
        assert!(!update_size_and_scale(
            &mut bounds,
            &mut scale,
            size,
            new_scale,
            |current| {
                wire.refresh(
                    current,
                    WindowBackgroundAppearance::Opaque,
                    WindowDecorations::Server,
                );
            }
        ));
        assert!(wire.requests().is_empty());
    }
}

#[test]
fn opaque_region_fractional_scale_change_redeclares_logical_size() {
    let mut wire = Wire::new();
    let mut bounds = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(900.0), px(600.0)),
    };
    let mut scale = 1.0;
    for new_scale in [1.25, 1.5, 2.0, 1.0] {
        assert!(update_size_and_scale(
            &mut bounds,
            &mut scale,
            None,
            Some(new_scale),
            |current| {
                wire.refresh(
                    current,
                    WindowBackgroundAppearance::Opaque,
                    WindowDecorations::Server,
                );
            }
        ));
        assert_eq!(scale, new_scale);
        wire.assert_opaque(900, 600);
    }
    assert!(update_size_and_scale(
        &mut bounds,
        &mut scale,
        Some(size(px(1000.0), px(700.0))),
        Some(1.25),
        |current| {
            wire.refresh(
                current,
                WindowBackgroundAppearance::Opaque,
                WindowDecorations::Server,
            );
        }
    ));
    assert_eq!(scale, 1.25);
    wire.assert_opaque(1000, 700);
}
