/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::convert::TryInto;

pub(super) fn is_quad_shader(name: &str) -> bool {
    name.starts_with("ps_quad_") && name != "ps_quad_mask"
}

pub(super) fn packed_size(input: &[u8]) -> Result<usize, String> {
    if input.len() % 16 != 0 {
        return Err("Invalid Vulkan quad instance stride".into());
    }
    let mut size = input.len();
    for instance in input.chunks_exact(16) {
        let word = u32::from_ne_bytes(instance[8..12].try_into().unwrap());
        let part = (word >> 8) & 255;
        if (word >> 24) & 8 != 0 && (part == 1 || part == 3) {
            size = size
                .checked_add(((word >> 16) & 10).count_ones() as usize * 16)
                .ok_or("Vulkan instance size overflow")?;
        }
    }
    Ok(size)
}

// Match vertices at AA strip joins to prevent subpixel rasterization gaps.
pub(super) fn pack(input: &[u8], output: &mut [u8]) {
    let mut cursor = 0;
    for instance in input.chunks_exact(16) {
        let word = u32::from_ne_bytes(instance[8..12].try_into().unwrap());
        let part = (word >> 8) & 255;
        if (word >> 24) & 8 != 0 && (part == 1 || part == 3) {
            for replacement in [
                if part == 1 { 6 } else { 8 },
                part,
                if part == 1 { 7 } else { 9 },
            ] {
                if replacement != part {
                    let edge = if replacement == 6 || replacement == 8 {
                        2
                    } else {
                        8
                    };
                    if (word >> 16) & edge == 0 {
                        continue;
                    }
                }
                let dst = &mut output[cursor..cursor + 16];
                dst[..8].copy_from_slice(&instance[..8]);
                dst[8..12].copy_from_slice(&((word & !0xff00) | (replacement << 8)).to_ne_bytes());
                dst[12..].copy_from_slice(&instance[12..]);
                cursor += 16;
            }
        } else {
            output[cursor..cursor + 16].copy_from_slice(instance);
            cursor += 16;
        }
    }
    assert_eq!(cursor, output.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staged_quad_instances_preserve_aa_join_order() {
        assert!(is_quad_shader("ps_quad_textured"));
        assert!(!is_quad_shader("ps_quad_mask"));
        assert!(!is_quad_shader("ps_text_run"));
        assert!(packed_size(&[0; 15]).is_err());
        for part in 0u32..=5 {
            for edges in 0u32..16 {
                for aa in [0u32, 8] {
                    let word = (aa << 24) | (edges << 16) | (part << 8) | 17;
                    let words = [13u32, 23, word, 47].map(u32::to_ne_bytes);
                    let source = words.as_flattened();
                    let mut output = vec![0; packed_size(source).unwrap()];
                    pack(source, &mut output);
                    let mut expected = Vec::new();
                    if aa != 0 && (part == 1 || part == 3) && edges & 2 != 0 {
                        expected.push(if part == 1 { 6 } else { 8 });
                    }
                    expected.push(part);
                    if aa != 0 && (part == 1 || part == 3) && edges & 8 != 0 {
                        expected.push(if part == 1 { 7 } else { 9 });
                    }
                    assert_eq!(output.len(), expected.len() * 16);
                    for (instance, part) in output.chunks_exact(16).zip(expected) {
                        assert_eq!(&instance[..8], &source[..8]);
                        assert_eq!(&instance[12..], &source[12..]);
                        assert_eq!(
                            u32::from_ne_bytes(instance[8..12].try_into().unwrap()),
                            (word & !0xff00) | (part << 8)
                        );
                    }
                }
            }
        }
    }
}
