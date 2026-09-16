/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignRgbFormat {
    Rgba8,
    Bgra8,
}

impl ForeignRgbFormat {
    pub fn from_drm_fourcc(fourcc: u32) -> Result<Self, &'static str> {
        match fourcc {
            0x34324241 => Ok(Self::Rgba8),
            0x34325241 => Ok(Self::Bgra8),
            _ => Err("Foreign DMA-BUF requires DRM ABGR8888 or ARGB8888"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForeignRgbLayout {
    size: [u32; 2],
    format: ForeignRgbFormat,
    stride: u64,
    offset: u64,
    required_bytes: u64,
}

impl ForeignRgbLayout {
    pub fn new(
        size: [u32; 2],
        fourcc: u32,
        modifier: u64,
        stride: u64,
        offset: u64,
    ) -> Result<Self, &'static str> {
        let format = ForeignRgbFormat::from_drm_fourcc(fourcc)?;
        if modifier != 0 {
            return Err("Foreign RGB DMA-BUF currently requires DRM_FORMAT_MOD_LINEAR");
        }
        if size.contains(&0) || size.iter().any(|&value| value > i32::MAX as u32) {
            return Err("Invalid foreign RGB DMA-BUF dimensions");
        }
        let row = u64::from(size[0]) * 4;
        if stride < row || stride % 4 != 0 || offset % 4 != 0 {
            return Err("Invalid foreign RGB DMA-BUF pitch or offset");
        }
        let required_bytes = stride
            .checked_mul(u64::from(size[1] - 1))
            .and_then(|value| value.checked_add(row))
            .and_then(|value| value.checked_add(offset))
            .ok_or("Foreign RGB DMA-BUF layout overflow")?;
        Ok(Self {
            size,
            format,
            stride,
            offset,
            required_bytes,
        })
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }
    pub fn format(&self) -> ForeignRgbFormat {
        self.format
    }
    pub fn stride(&self) -> u64 {
        self.stride
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn validate_allocation(&self, bytes: u64) -> Result<(), &'static str> {
        if self.required_bytes > bytes {
            return Err("Foreign RGB DMA-BUF layout exceeds its allocation");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_channels_match_little_endian_storage() {
        assert_eq!(
            ForeignRgbFormat::from_drm_fourcc(u32::from_le_bytes(*b"AB24")),
            Ok(ForeignRgbFormat::Rgba8)
        );
        assert_eq!(
            ForeignRgbFormat::from_drm_fourcc(u32::from_le_bytes(*b"AR24")),
            Ok(ForeignRgbFormat::Bgra8)
        );
        for format in [*b"XB24", *b"XR24", *b"NV12", *b"P010"] {
            assert!(ForeignRgbFormat::from_drm_fourcc(u32::from_le_bytes(format)).is_err());
        }
    }

    #[test]
    fn padding_and_offset_require_the_last_pixel_not_trailing_padding() {
        let layout =
            ForeignRgbLayout::new([3, 2], u32::from_le_bytes(*b"AB24"), 0, 64, 128).unwrap();
        assert!(layout.validate_allocation(203).is_err());
        assert!(layout.validate_allocation(204).is_ok());
        assert!(layout.validate_allocation(256).is_ok());
        assert_eq!(layout.size(), [3, 2]);
        assert_eq!(layout.format(), ForeignRgbFormat::Rgba8);
        assert_eq!(layout.stride(), 64);
        assert_eq!(layout.offset(), 128);
    }

    #[test]
    fn one_row_does_not_require_another_pitch() {
        let layout = ForeignRgbLayout::new([1, 1], 0x34325241, 0, 4096, 4).unwrap();
        assert!(layout.validate_allocation(7).is_err());
        assert!(layout.validate_allocation(8).is_ok());
    }

    #[test]
    fn rejects_tiled_and_implicit_modifiers() {
        for modifier in [1, 0x0100_0000_0000_0001, 0x00ff_ffff_ffff_ffff, u64::MAX] {
            assert!(ForeignRgbLayout::new([8, 8], 0x34324241, modifier, 32, 0).is_err());
        }
    }

    #[test]
    fn rejects_empty_and_unrepresentable_sizes() {
        for size in [[0, 1], [1, 0], [u32::MAX, 1], [1, u32::MAX]] {
            assert!(ForeignRgbLayout::new(size, 0x34324241, 0, u64::MAX - 3, 0).is_err());
        }
    }

    #[test]
    fn rejects_short_and_unaligned_rows() {
        for (stride, offset) in [(0, 0), (28, 0), (33, 0), (32, 1)] {
            assert!(ForeignRgbLayout::new([8, 2], 0x34324241, 0, stride, offset).is_err());
        }
    }

    #[test]
    fn rejects_overflow_before_allocation_check() {
        for (size, stride, offset) in [
            ([1, 1], 4, u64::MAX - 3),
            ([1, 3], u64::MAX - 3, 0),
            ([1, 2], 4, u64::MAX - 7),
        ] {
            assert!(ForeignRgbLayout::new(size, 0x34324241, 0, stride, offset).is_err());
        }
    }
}
