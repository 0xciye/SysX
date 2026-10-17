//! JPEG decoding through libjpeg-turbo (SIMD), shared by the backend and the
//! native monitor. The header is checked against the expected size before
//! any pixel buffer is allocated.

use std::cell::RefCell;
use turbojpeg::{Decompressor, Image, PixelFormat};

/// Largest side either decoder accepts.
pub const MAX_SIDE: u32 = 8192;

#[derive(Debug)]
pub enum JpegError {
    Decode(String),
    Dimensions { width: u32, height: u32 },
}

impl std::fmt::Display for JpegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(message) => f.write_str(message),
            Self::Dimensions { width, height } => write!(f, "JPEG is {width}x{height}"),
        }
    }
}

thread_local! {
    static DECOMPRESSOR: RefCell<Option<Decompressor>> = const { RefCell::new(None) };
}

/// Decodes `jpeg` as packed RGB (`rgba == false`) or RGBA with opaque alpha.
/// Fails with [`JpegError::Dimensions`] (the JPEG's real size) when it is not
/// `width` × `height`.
pub fn decode(jpeg: &[u8], width: u32, height: u32, rgba: bool) -> Result<Vec<u8>, JpegError> {
    DECOMPRESSOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(Decompressor::new().map_err(|e| JpegError::Decode(e.to_string()))?);
        }
        let decompressor = slot.as_mut().expect("decompressor");
        let header = decompressor
            .read_header(jpeg)
            .map_err(|e| JpegError::Decode(e.to_string()))?;
        let (actual_width, actual_height) = (header.width as u32, header.height as u32);
        if (actual_width, actual_height) != (width, height) || width > MAX_SIDE || height > MAX_SIDE
        {
            return Err(JpegError::Dimensions {
                width: actual_width,
                height: actual_height,
            });
        }
        let (format, channels) = if rgba {
            (PixelFormat::RGBA, 4)
        } else {
            (PixelFormat::RGB, 3)
        };
        let (width, height) = (width as usize, height as usize);
        let mut pixels = vec![0; width * height * channels];
        decompressor
            .decompress(
                jpeg,
                Image {
                    pixels: pixels.as_mut_slice(),
                    width,
                    pitch: width * channels,
                    height,
                    format,
                },
            )
            .map_err(|e| JpegError::Decode(e.to_string()))?;
        Ok(pixels)
    })
}
