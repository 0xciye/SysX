use image::{imageops, RgbImage};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CropError {
    #[error("crop resolution must be positive")]
    InvalidResolution,
    #[error("frame is {width}x{height}; crop resolution {resolution}x{resolution} does not fit")]
    TooSmall {
        width: u32,
        height: u32,
        resolution: u32,
    },
}

pub fn centered_crop_rect(
    width: u32,
    height: u32,
    resolution: Option<u32>,
) -> Result<CropRect, CropError> {
    let (crop_width, crop_height) = match resolution {
        None => (width, height),
        Some(0) => return Err(CropError::InvalidResolution),
        Some(side) if side > width || side > height => {
            return Err(CropError::TooSmall {
                width,
                height,
                resolution: side,
            });
        }
        Some(side) => (side, side),
    };
    if width == 0 || height == 0 {
        return Err(CropError::TooSmall {
            width,
            height,
            resolution: resolution.unwrap_or(1),
        });
    }
    Ok(CropRect {
        x: (width - crop_width) / 2,
        y: (height - crop_height) / 2,
        width: crop_width,
        height: crop_height,
    })
}

pub fn centered_crop(
    image: &RgbImage,
    resolution: Option<u32>,
) -> Result<(RgbImage, CropRect), CropError> {
    let rect = centered_crop_rect(image.width(), image.height(), resolution)?;
    let cropped = imageops::crop_imm(image, rect.x, rect.y, rect.width, rect.height).to_image();
    Ok((cropped, rect))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn centered_square_crop_returns_source_origin() {
        let image = RgbImage::from_pixel(8, 6, Rgb([1, 2, 3]));
        let (crop, rect) = centered_crop(&image, Some(4)).unwrap();
        assert_eq!(
            rect,
            CropRect {
                x: 2,
                y: 1,
                width: 4,
                height: 4
            }
        );
        assert_eq!((crop.width(), crop.height()), (4, 4));
    }

    #[test]
    fn crop_rejects_oversized_and_zero_resolution() {
        assert!(matches!(
            centered_crop_rect(10, 8, Some(9)),
            Err(CropError::TooSmall { .. })
        ));
        assert_eq!(
            centered_crop_rect(10, 8, Some(0)),
            Err(CropError::InvalidResolution)
        );
    }
}
