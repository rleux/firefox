/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
fn storage_binding_remap_leaves_other_decorations_unchanged() {
    let words = [
        0x07230203u32,
        0x00010300,
        0,
        8,
        0,
        (4 << 16) | 71,
        7,
        34,
        0,
        (4 << 16) | 71,
        7,
        33,
        7,
        (4 << 16) | 71,
        3,
        6,
        16,
    ];
    let bytes: Vec<_> = words.iter().copied().flat_map(u32::to_le_bytes).collect();
    let remapped = remap_bindings(&bytes, &BTreeMap::from([(7, 2)]));
    let mut expected = bytes;
    expected[12 * 4..13 * 4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(remapped, expected);
}
