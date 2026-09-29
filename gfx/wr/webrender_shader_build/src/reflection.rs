/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::collections::BTreeMap;
use std::convert::TryInto;

#[cfg(test)]
mod tests;

pub(super) fn remap_bindings(bytes: &[u8], bindings: &BTreeMap<u32, u32>) -> Vec<u8> {
    assert_eq!(bytes.len() % 4, 0);
    let mut words: Vec<_> = bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    assert_eq!(words[0], 0x07230203);
    let mut offset = 5;
    while offset < words.len() {
        let count = (words[offset] >> 16) as usize;
        assert!(count > 0 && offset + count <= words.len());
        // OpDecorate Binding must match HAL's dense native descriptor numbering.
        if words[offset] & 0xffff == 71 && count >= 3 && words[offset + 2] == 33 {
            assert_eq!(count, 4);
            words[offset + 3] = bindings[&words[offset + 3]];
        }
        offset += count;
    }
    words.into_iter().flat_map(u32::to_le_bytes).collect()
}

#[derive(Debug, PartialEq)]
pub(super) struct Interface {
    pub location: u32,
    pub scalar: &'static str,
    pub components: u32,
    pub locations: u32,
    pub flat: bool,
    pub interpolation: u8,
    pub index: u32,
}

#[derive(Default)]
pub(super) struct Reflection {
    pub inputs: BTreeMap<String, Interface>,
    pub outputs: BTreeMap<String, Interface>,
    pub textures: BTreeMap<u32, (String, &'static str)>,
    pub storage_buffers: BTreeMap<u32, (String, &'static str)>,
    pub samplers: Vec<u32>,
    pub projection: bool,
}

#[cfg(feature = "glslang")]
mod glslang;
#[cfg(feature = "glslang")]
pub(super) use glslang::reflect;
