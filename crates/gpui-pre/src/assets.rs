use crate::{DevicePixels, Pixels, Result, SharedString, Size, size};
use smallvec::SmallVec;

use image::{Delay, Frame};
use std::{
    borrow::Cow,
    fmt,
    hash::Hash,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};

/// A source of assets for this app to use.
pub trait AssetSource: 'static + Send + Sync {
    /// Load the given asset from the source path.
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>>;

    /// List the assets at the given path.
    fn list(&self, path: &str) -> Result<Vec<SharedString>>;
}

impl AssetSource for () {
    fn load(&self, _path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(None)
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

/// A unique identifier for the image cache
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ImageId(pub usize);

#[derive(PartialEq, Eq, Hash, Clone)]
#[expect(missing_docs)]
pub struct RenderImageParams {
    pub image_id: ImageId,
    pub frame_index: usize,
}

/// A cached and processed image, in BGRA format
pub struct RenderImage {
    /// The ID associated with this image
    pub id: ImageId,
    /// The scale factor of this image on render.
    pub(crate) scale_factor: f32,
    data: SmallVec<[Frame; 1]>,
}

impl PartialEq for RenderImage {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RenderImage {}

impl RenderImage {
    /// Create a new image from the given data.
    pub fn new(data: impl Into<SmallVec<[Frame; 1]>>) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        Self {
            id: ImageId(NEXT_ID.fetch_add(1, SeqCst)),
            scale_factor: 1.0,
            data: data.into(),
        }
    }

    /// Convert this image into a byte slice.
    pub fn as_bytes(&self, frame_index: usize) -> Option<&[u8]> {
        self.data
            .get(frame_index)
            .map(|frame| frame.buffer().as_raw().as_slice())
    }

    /// Get the size of this image, in pixels.
    pub fn size(&self, frame_index: usize) -> Size<DevicePixels> {
        self.data
            .get(frame_index)
            .map(|frame| {
                let (width, height) = frame.buffer().dimensions();
                size(width.into(), height.into())
            })
            .unwrap_or_default()
    }

    /// Get the size of this image, in pixels for display, adjusted for the scale factor.
    pub(crate) fn render_size(&self, frame_index: usize) -> Size<Pixels> {
        self.size(frame_index)
            .map(|v| (v.0 as f32 / self.scale_factor).into())
    }

    /// Get the delay of this frame from the previous
    pub fn delay(&self, frame_index: usize) -> Delay {
        self.data
            .get(frame_index)
            .map(|frame| frame.delay())
            .unwrap_or(Delay::from_numer_denom_ms(100, 1))
    }

    /// Get the number of frames for this image.
    pub fn frame_count(&self) -> usize {
        self.data.len()
    }

    /// Create a one-frame image from BGRA pixels, `width` by `height` with rows
    /// packed, taking the buffer as it is so nothing is decoded or copied.
    /// Returns `None` unless `bytes` holds exactly `width * height * 4` bytes.
    pub fn from_bgra(width: u32, height: u32, bytes: Vec<u8>) -> Option<Self> {
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if bytes.len() != len {
            return None;
        }
        let buffer = image::RgbaImage::from_raw(width, height, bytes)?;
        Some(Self::new([Frame::new(buffer)]))
    }

    /// Take back the pixel buffer of a one-frame image, so a caller that draws
    /// a new image every frame can reuse its allocation. Returns `None` for an
    /// image with any other number of frames.
    pub fn into_bgra(self) -> Option<Vec<u8>> {
        let mut data = self.data;
        if data.len() != 1 {
            return None;
        }
        data.pop().map(|frame| frame.into_buffer().into_raw())
    }
}

impl fmt::Debug for RenderImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageData")
            .field("id", &self.id)
            .field("size", &self.data.first().map(|f| f.buffer().dimensions()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::SmallVec;

    #[test]
    fn empty_render_image_does_not_panic() {
        let image = RenderImage::new(SmallVec::new());
        assert_eq!(image.frame_count(), 0);
        assert_eq!(image.size(0), Size::default());
        assert_eq!(image.as_bytes(0), None);
        assert_eq!(image.render_size(0), Size::default());
        assert_eq!(image.delay(0), Delay::from_numer_denom_ms(100, 1));
        let _ = format!("{image:?}");
    }

    #[test]
    fn bgra_pixels_become_an_image_without_a_copy_and_come_back() {
        let bytes: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8).collect();
        let pointer = bytes.as_ptr();
        let image = RenderImage::from_bgra(3, 2, bytes.clone()).expect("exact size");
        assert_eq!(image.frame_count(), 1);
        assert_eq!(image.size(0), size(DevicePixels(3), DevicePixels(2)));
        assert_eq!(image.as_bytes(0), Some(bytes.as_slice()));
        let moved = RenderImage::from_bgra(3, 2, bytes).expect("exact size");
        assert_eq!(moved.as_bytes(0).map(<[u8]>::as_ptr), Some(pointer));
        let back = moved.into_bgra().expect("one frame");
        assert_eq!(back.as_ptr(), pointer);
        assert_eq!(back.len(), 24);
    }

    #[test]
    fn bgra_pixels_of_the_wrong_size_are_refused() {
        assert!(RenderImage::from_bgra(3, 2, vec![0; 23]).is_none());
        assert!(RenderImage::from_bgra(3, 2, vec![0; 25]).is_none());
        assert!(RenderImage::from_bgra(u32::MAX, u32::MAX, Vec::new()).is_none());
        assert!(RenderImage::from_bgra(0, 0, Vec::new()).is_some());
        assert_eq!(RenderImage::new(SmallVec::new()).into_bgra(), None);
    }
}
