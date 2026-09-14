/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub(super) fn iosurface_format(
    pixel_format: u32,
    count: usize,
    plane: usize,
) -> Result<api::ImageFormat> {
    if count == 0 && plane == 0 {
        return match pixel_format.to_be_bytes() {
            [b'B', b'G', b'R', b'A'] => Ok(api::ImageFormat::BGRA8),
            [b'R', b'G', b'B', b'A'] => Ok(api::ImageFormat::RGBA8),
            _ => Err("Unsupported non-planar IOSurface pixel format".into()),
        };
    }
    if count != 2 || plane >= count {
        return Err("Unsupported IOSurface plane layout".into());
    }
    match pixel_format.to_be_bytes() {
        [b'4', b'2', b'0', b'v' | b'f'] => Ok(if plane == 0 {
            api::ImageFormat::R8
        } else {
            api::ImageFormat::RG8
        }),
        [b'x', b'4', b'2', b'0'] | [b'x', b'f', b'2', b'0'] => Ok(if plane == 0 {
            api::ImageFormat::R16
        } else {
            api::ImageFormat::RG16
        }),
        _ => Err("Unsupported planar IOSurface pixel format".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn iosurface_plane_formats_reject_illegal_reinterpretation() {
        for tag in [*b"420v", *b"420f"] {
            assert_eq!(
                iosurface_format(u32::from_be_bytes(tag), 2, 0).unwrap(),
                api::ImageFormat::R8
            );
            assert_eq!(
                iosurface_format(u32::from_be_bytes(tag), 2, 1).unwrap(),
                api::ImageFormat::RG8
            );
        }
        for tag in [*b"x420", *b"xf20"] {
            assert_eq!(
                iosurface_format(u32::from_be_bytes(tag), 2, 0).unwrap(),
                api::ImageFormat::R16
            );
            assert_eq!(
                iosurface_format(u32::from_be_bytes(tag), 2, 1).unwrap(),
                api::ImageFormat::RG16
            );
        }
        assert_eq!(
            iosurface_format(u32::from_be_bytes(*b"BGRA"), 0, 0).unwrap(),
            api::ImageFormat::BGRA8
        );
        assert!(iosurface_format(u32::from_be_bytes(*b"BGRA"), 2, 0).is_err());
        assert!(iosurface_format(u32::from_be_bytes(*b"420v"), 2, 2).is_err());
        assert!(iosurface_format(0, 0, 0).is_err());
    }
}
