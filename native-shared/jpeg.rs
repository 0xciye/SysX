//! JPEG decoding through libjpeg-turbo (SIMD), shared by the backend and the
//! native monitor. The header is checked against the expected size before
//! any pixel buffer is allocated.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use turbojpeg::{Decompressor, Image, PixelFormat, ScalingFactor};

/// Largest side either decoder accepts.
pub const MAX_SIDE: u32 = 8192;
/// Matches the receiver's maximum accepted frame size before allocating pixels.
pub const MAX_PIXELS: u64 = 33_554_432;

fn dimensions_within_limit(width: u32, height: u32) -> bool {
    width != 0
        && height != 0
        && width <= MAX_SIDE
        && height <= MAX_SIDE
        && u64::from(width) * u64::from(height) <= MAX_PIXELS
}

fn preview_factor(width: u32, height: u32, max_side: u32) -> ScalingFactor {
    (1..=8)
        .rev()
        .map(|eighths| ScalingFactor::new(eighths, 8))
        .find(|factor| factor.scale(width.max(height) as usize) <= max_side as usize)
        .unwrap_or(ScalingFactor::ONE_EIGHTH)
}

pub fn preview_dimensions(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let factor = preview_factor(width, height, max_side);
    (
        factor.scale(width as usize) as u32,
        factor.scale(height as usize) as u32,
    )
}

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
    with_decompressor(
        jpeg,
        width,
        height,
        rgba,
        |decompressor, width, height, format, channels, len| {
            let mut pixels = Vec::new();
            pixels.try_reserve_exact(len).map_err(|error| {
                JpegError::Decode(format!("could not allocate JPEG output buffer: {error}"))
            })?;
            pixels.resize(len, 0);
            decompress_pixels(
                decompressor,
                jpeg,
                width,
                height,
                format,
                channels,
                &mut pixels,
            )?;
            Ok(pixels)
        },
    )
}

/// Reads and validates the JPEG dimensions without allocating a pixel buffer.
pub fn dimensions(jpeg: &[u8]) -> Result<(u32, u32), JpegError> {
    DECOMPRESSOR.with(|slot| {
        let mut slot = slot.try_borrow_mut().map_err(|error| {
            JpegError::Decode(format!("JPEG decoder is already borrowed: {error}"))
        })?;
        if slot.is_none() {
            *slot =
                Some(Decompressor::new().map_err(|error| JpegError::Decode(error.to_string()))?);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            (|| {
                let header = slot
                    .as_mut()
                    .ok_or_else(|| JpegError::Decode("JPEG decoder initialization failed".into()))?
                    .read_header(jpeg)
                    .map_err(|error| JpegError::Decode(error.to_string()))?;
                let dimensions = (header.width as u32, header.height as u32);
                if !dimensions_within_limit(dimensions.0, dimensions.1) {
                    return Err(JpegError::Dimensions {
                        width: dimensions.0,
                        height: dimensions.1,
                    });
                }
                Ok(dimensions)
            })()
        }));
        let result = result.unwrap_or_else(|panic| Err(decoder_panic(panic)));
        if result.is_err() {
            *slot = None;
        }
        result
    })
}

/// Decodes directly into a caller-owned packed RGB/RGBA buffer.
/// The JPEG header, dimensions and output byte count are validated before it
/// writes, so a native preview buffer can be decoded without an intermediate copy.
pub fn decode_into(
    jpeg: &[u8],
    width: u32,
    height: u32,
    rgba: bool,
    pixels: &mut [u8],
) -> Result<(), JpegError> {
    with_decompressor(
        jpeg,
        width,
        height,
        rgba,
        |decompressor, width, height, format, channels, len| {
            if pixels.len() != len {
                return Err(JpegError::Decode(format!(
                    "JPEG output buffer has {} bytes, expected {len}",
                    pixels.len()
                )));
            }
            decompress_pixels(decompressor, jpeg, width, height, format, channels, pixels)
        },
    )
}

/// Decodes a monitor preview at TurboJPEG's nearest supported size no larger
/// than `max_side`, keeping detection metadata in its original coordinates.
pub fn decode_preview_into(
    jpeg: &[u8],
    width: u32,
    height: u32,
    max_side: u32,
    rgba: bool,
    pixels: &mut [u8],
) -> Result<(u32, u32), JpegError> {
    DECOMPRESSOR.with(|slot| {
        let mut slot = slot.try_borrow_mut().map_err(|error| {
            JpegError::Decode(format!("JPEG decoder is already borrowed: {error}"))
        })?;
        if slot.is_none() {
            *slot =
                Some(Decompressor::new().map_err(|error| JpegError::Decode(error.to_string()))?);
        }
        let decompressor = slot
            .as_mut()
            .ok_or_else(|| JpegError::Decode("JPEG decoder initialization failed".into()))?;
        let result = catch_unwind(AssertUnwindSafe(|| {
            (|| {
                decompressor
                    .set_scaling_factor(ScalingFactor::ONE)
                    .map_err(|error| JpegError::Decode(error.to_string()))?;
                let header = decompressor
                    .read_header(jpeg)
                    .map_err(|error| JpegError::Decode(error.to_string()))?;
                if (header.width as u32, header.height as u32) != (width, height)
                    || !dimensions_within_limit(width, height)
                {
                    return Err(JpegError::Dimensions {
                        width: header.width as u32,
                        height: header.height as u32,
                    });
                }
                if max_side == 0 {
                    return Err(JpegError::Decode("invalid preview dimension limit".into()));
                }
                let factor = preview_factor(width, height, max_side);
                if header.is_lossless && factor != ScalingFactor::ONE {
                    return Err(JpegError::Decode(
                        "large lossless JPEG previews are not supported".into(),
                    ));
                }
                decompressor
                    .set_scaling_factor(factor)
                    .map_err(|error| JpegError::Decode(error.to_string()))?;
                let output_width = factor.scale(width as usize);
                let output_height = factor.scale(height as usize);
                let channels = if rgba { 4 } else { 3 };
                let expected = output_width
                    .checked_mul(output_height)
                    .and_then(|size| size.checked_mul(channels))
                    .ok_or_else(|| JpegError::Decode("JPEG output size overflow".into()))?;
                if pixels.len() != expected {
                    return Err(JpegError::Decode(format!(
                        "JPEG output buffer has {} bytes, expected {expected}",
                        pixels.len()
                    )));
                }
                let (format, channels) = if rgba {
                    (PixelFormat::RGBA, 4)
                } else {
                    (PixelFormat::RGB, 3)
                };
                decompress_pixels(
                    decompressor,
                    jpeg,
                    output_width,
                    output_height,
                    format,
                    channels,
                    pixels,
                )?;
                Ok((output_width as u32, output_height as u32))
            })()
        }));
        let reset = catch_unwind(AssertUnwindSafe(|| {
            decompressor
                .set_scaling_factor(ScalingFactor::ONE)
                .map_err(|error| JpegError::Decode(error.to_string()))
        }));
        let result = result.unwrap_or_else(|panic| Err(decoder_panic(panic)));
        let reset = reset.unwrap_or_else(|panic| Err(decoder_panic(panic)));
        if result.is_err() || reset.is_err() {
            *slot = None;
        }
        match (result, reset) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(JpegError::Decode(error.to_string())),
            (Ok(size), Ok(())) => Ok(size),
        }
    })
}

fn with_decompressor<T>(
    jpeg: &[u8],
    width: u32,
    height: u32,
    rgba: bool,
    decode: impl FnOnce(
        &mut Decompressor,
        usize,
        usize,
        PixelFormat,
        usize,
        usize,
    ) -> Result<T, JpegError>,
) -> Result<T, JpegError> {
    DECOMPRESSOR.with(|slot| {
        let mut slot = slot.try_borrow_mut().map_err(|error| {
            JpegError::Decode(format!("JPEG decoder is already borrowed: {error}"))
        })?;
        if slot.is_none() {
            *slot = Some(Decompressor::new().map_err(|e| JpegError::Decode(e.to_string()))?);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            (|| {
                let decompressor = slot.as_mut().ok_or_else(|| {
                    JpegError::Decode("JPEG decoder initialization failed".into())
                })?;
                let header = decompressor
                    .read_header(jpeg)
                    .map_err(|e| JpegError::Decode(e.to_string()))?;
                let (actual_width, actual_height) = (header.width as u32, header.height as u32);
                if (actual_width, actual_height) != (width, height)
                    || !dimensions_within_limit(width, height)
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
                let output_len = width
                    .checked_mul(height)
                    .and_then(|pixels| pixels.checked_mul(channels))
                    .ok_or_else(|| JpegError::Decode("JPEG output size overflow".into()))?;
                decode(decompressor, width, height, format, channels, output_len)
            })()
        }));
        let result = result.unwrap_or_else(|panic| Err(decoder_panic(panic)));
        if result.is_err() {
            // libjpeg may leave its state unusable after malformed input.
            *slot = None;
        }
        result
    })
}

fn decoder_panic(panic: Box<dyn std::any::Any + Send>) -> JpegError {
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic");
    JpegError::Decode(format!("JPEG decoder panicked: {message}"))
}

fn decompress_pixels(
    decompressor: &mut Decompressor,
    jpeg: &[u8],
    width: usize,
    height: usize,
    format: PixelFormat,
    channels: usize,
    pixels: &mut [u8],
) -> Result<(), JpegError> {
    decompressor
        .decompress(
            jpeg,
            Image {
                pixels,
                width,
                pitch: width * channels,
                height,
                format,
            },
        )
        .map_err(|error| JpegError::Decode(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{
        dimensions, dimensions_within_limit, preview_dimensions, with_decompressor, JpegError,
    };

    const JPEG: &[u8] = include_bytes!("../src/assets/red-cube-192.jpg");

    #[test]
    fn jpeg_output_dimensions_obey_receiver_pixel_budget() {
        assert!(dimensions_within_limit(640, 640));
        assert!(dimensions_within_limit(8192, 4096));
        assert!(!dimensions_within_limit(8192, 8192));
        assert!(!dimensions_within_limit(0, 640));
    }

    #[test]
    fn monitor_preview_uses_the_largest_supported_scale_within_its_side_limit() {
        assert_eq!(preview_dimensions(8192, 4096, 1280), (1024, 512));
        assert_eq!(preview_dimensions(2000, 1000, 1280), (1250, 625));
        assert_eq!(preview_dimensions(640, 480, 1280), (640, 480));
        let (width, height) = preview_dimensions(4096, 8192, 1280);
        assert!(width.max(height) <= 1280);
    }

    #[test]
    fn decoder_panic_discards_thread_local_state() {
        let result = with_decompressor(JPEG, 192, 192, false, |_, _, _, _, _, _| {
            panic!("simulated decoder panic");
            #[allow(unreachable_code)]
            Ok::<(), JpegError>(())
        });
        assert!(
            matches!(result, Err(JpegError::Decode(message)) if message.contains("simulated decoder panic"))
        );
        assert_eq!(dimensions(JPEG).unwrap(), (192, 192));
    }
}
