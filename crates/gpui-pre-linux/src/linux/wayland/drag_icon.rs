use std::{
    fs::File,
    io::Write,
    os::fd::{AsFd, FromRawFd},
};

use gpui::ExternalDragImage;
use wayland_client::{
    Proxy,
    protocol::{wl_buffer::WlBuffer, wl_shm, wl_shm_pool::WlShmPool, wl_surface::WlSurface},
};

use super::Globals;

pub(super) const PENDING_ICON_LIMIT: usize = 4;

/// Whether a new drag may carry an icon while this many retired icons still wait for release.
pub(super) fn icon_allowed(retired: usize) -> bool {
    retired < PENDING_ICON_LIMIT
}

// Surface lifetime follows the gesture; buffer lifetime follows the compositor's release.
struct IconLease<S, B> {
    surface: Option<S>,
    buffer: Option<B>,
}

impl<S, B> IconLease<S, B> {
    fn finish(&mut self) {
        self.surface.take();
    }
    fn release(&mut self) {
        self.buffer.take();
    }
    fn done(&self) -> bool {
        self.surface.is_none() && self.buffer.is_none()
    }
}

struct IconSurface(WlSurface);
impl Drop for IconSurface {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

struct IconBuffer {
    proxy: WlBuffer,
    pool: WlShmPool,
    _file: File,
}
impl Drop for IconBuffer {
    fn drop(&mut self) {
        self.proxy.destroy();
        self.pool.destroy();
    }
}

pub(super) struct DragIcon {
    lease: IconLease<IconSurface, IconBuffer>,
}

fn image_file(image: &ExternalDragImage) -> std::io::Result<File> {
    // SAFETY: the constant is NUL-terminated and flags request an owned, close-on-exec descriptor.
    let fd = unsafe {
        libc::memfd_create(
            c"gpui-drag-icon".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned this fresh descriptor and no other File owns it.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::with_capacity(image.rgba().len());
    for rgba in image.rgba().chunks_exact(4) {
        let alpha = u32::from(rgba[3]);
        let premultiply = |value: u8| (u32::from(value) * alpha + 127) / 255;
        let argb = alpha << 24
            | premultiply(rgba[0]) << 16
            | premultiply(rgba[1]) << 8
            | premultiply(rgba[2]);
        bytes.extend_from_slice(&argb.to_ne_bytes());
    }
    file.write_all(&bytes)?;
    // Compositors require shared writable mappings; freeze the backing size without write seals.
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    // SAFETY: fd remains owned by file; F_ADD_SEALS reads only the integer bit mask.
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

impl DragIcon {
    pub(super) fn new(globals: &Globals, image: &ExternalDragImage) -> std::io::Result<Self> {
        let file = image_file(image)?;
        let pool =
            globals
                .shm
                .create_pool(file.as_fd(), image.rgba().len() as i32, &globals.qh, ());
        let buffer = pool.create_buffer(
            0,
            image.width() as i32,
            image.height() as i32,
            (image.width() * 4) as i32,
            wl_shm::Format::Argb8888,
            &globals.qh,
            (),
        );
        let surface = globals.compositor.create_surface(&globals.qh, ());
        surface.set_buffer_scale(image.scale() as i32);
        let region = globals.compositor.create_region(&globals.qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let (x, y) = image.hotspot();
        if surface.version() >= 5 {
            surface.offset(-x, -y);
            surface.attach(Some(&buffer), 0, 0);
        } else {
            surface.attach(Some(&buffer), -x, -y);
        }
        if surface.version() >= 4 {
            surface.damage_buffer(0, 0, image.width() as i32, image.height() as i32);
        } else {
            surface.damage(
                -x,
                -y,
                (image.width() / image.scale()) as i32,
                (image.height() / image.scale()) as i32,
            );
        }
        Ok(Self {
            lease: IconLease {
                surface: Some(IconSurface(surface)),
                buffer: Some(IconBuffer {
                    proxy: buffer,
                    pool,
                    _file: file,
                }),
            },
        })
    }

    pub(super) fn surface(&self) -> &WlSurface {
        &self.lease.surface.as_ref().unwrap().0
    }
    pub(super) fn finish(&mut self) {
        self.lease.finish();
    }
    pub(super) fn release(&mut self, buffer: &WlBuffer) {
        if self
            .lease
            .buffer
            .as_ref()
            .is_some_and(|held| held.proxy == *buffer)
        {
            self.lease.release();
        }
    }
    pub(super) fn done(&self) -> bool {
        self.lease.done()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        io::{Read, Seek},
        rc::Rc,
    };

    struct Tracked(Rc<Cell<usize>>);
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn icon_allowed_stops_at_the_retired_cap() {
        assert!(icon_allowed(PENDING_ICON_LIMIT - 1));
        assert!(!icon_allowed(PENDING_ICON_LIMIT));
    }

    #[test]
    fn icon_finish_cancel_source_teardown_wait_for_release() {
        for release_first in [false, true] {
            for _terminal in ["finished", "cancelled", "source closed"] {
                let surfaces = Rc::new(Cell::new(0));
                let buffers = Rc::new(Cell::new(0));
                let mut lease = IconLease {
                    surface: Some(Tracked(surfaces.clone())),
                    buffer: Some(Tracked(buffers.clone())),
                };
                if release_first {
                    lease.release();
                }
                lease.finish();
                lease.finish();
                assert_eq!(surfaces.get(), 1);
                assert_eq!(buffers.get(), usize::from(release_first));
                assert_eq!(lease.done(), release_first);
                lease.release();
                lease.release();
                assert!(lease.done());
                drop(lease);
                assert_eq!(buffers.get(), 1);
            }
        }
    }

    #[test]
    fn icon_connection_teardown_drops_unreleased_resources_once() {
        let dropped = Rc::new(Cell::new(0));
        drop(IconLease {
            surface: Some(Tracked(dropped.clone())),
            buffer: Some(Tracked(dropped.clone())),
        });
        assert_eq!(dropped.get(), 2);
    }

    #[test]
    fn icon_memfd_is_premultiplied_argb_and_sealed() {
        let image = ExternalDragImage::new(
            [200, 100, 50, 128, 255, 255, 255, 0].into(),
            2,
            1,
            1,
            (1, 0),
        )
        .unwrap();
        let mut file = image_file(&image).unwrap();
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            [0x80643219_u32.to_ne_bytes(), 0_u32.to_ne_bytes()].concat()
        );
        // Match libwayland-server's shared writable mapping of wl_shm pools.
        use std::os::fd::AsRawFd;
        // SAFETY: the live file owns at least bytes.len() bytes; the mapping is checked and unmapped below.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            mapping,
            libc::MAP_FAILED,
            "compositor cannot map the icon pool: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the preceding mmap succeeded for exactly bytes.len() bytes.
        assert_eq!(unsafe { libc::munmap(mapping, bytes.len()) }, 0);
        assert_eq!(file.metadata().unwrap().len(), bytes.len() as u64);
        // SAFETY: file owns this live descriptor; F_GET_SEALS has no pointer arguments.
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        assert!(seals >= 0);
        let size_seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
        assert_eq!(seals & size_seals, size_seals);
        assert_eq!(seals & (libc::F_SEAL_WRITE | libc::F_SEAL_FUTURE_WRITE), 0);
        assert!(
            file.set_len(bytes.len() as u64 + 4).is_err(),
            "icon backing must not grow"
        );
        assert_eq!(image.rgba().as_ref(), [200, 100, 50, 128, 255, 255, 255, 0]);
        assert!(
            file.set_len(0).is_err(),
            "compositor mapping must not be truncated"
        );
    }

    #[test]
    fn icon_write_seals_reject_required_compositor_mapping() {
        use std::os::fd::AsRawFd;
        for seal in [libc::F_SEAL_WRITE, libc::F_SEAL_FUTURE_WRITE] {
            // SAFETY: the constant is NUL-terminated and this call returns an owned descriptor.
            let fd = unsafe {
                libc::memfd_create(
                    c"gpui-icon-seal-control".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            assert!(fd >= 0);
            // SAFETY: fd is freshly created and no other File owns it.
            let file = unsafe { File::from_raw_fd(fd) };
            file.set_len(4).unwrap();
            // SAFETY: the descriptor is live and F_ADD_SEALS takes an integer mask.
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seal) },
                0
            );
            // SAFETY: this maps the file's four bytes; a successful control is unmapped below.
            let mapping = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            let error = std::io::Error::last_os_error();
            if mapping != libc::MAP_FAILED {
                // SAFETY: the preceding mmap succeeded for four bytes.
                unsafe { libc::munmap(mapping, 4) };
                panic!("write seal {seal} unexpectedly allowed compositor mapping");
            }
            assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        }
    }
}
