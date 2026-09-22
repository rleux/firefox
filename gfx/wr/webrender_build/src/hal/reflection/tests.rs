/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

const STORAGE: &str = r#"
OpName %7 "b_sGpuBufferF"
OpDecorate %3 ArrayStride 16
OpDecorate %4 Block
OpMemberDecorate %4 0 Offset 0
OpMemberDecorate %4 0 NonWritable
OpDecorate %7 DescriptorSet 0
OpDecorate %7 Binding 7
%1 = OpTypeFloat 32
%2 = OpTypeVector %1 4
%3 = OpTypeRuntimeArray %2
%4 = OpTypeStruct %3
%5 = OpTypePointer StorageBuffer %4
%7 = OpVariable %5 StorageBuffer
"#;

#[test]
fn readonly_storage_tables_reflect_scalar_and_binding() {
    for (source, scalar, name) in [
        (STORAGE.to_owned(), "Float", "sGpuBufferF"),
        (STORAGE.replace("OpTypeFloat 32", "OpTypeInt 32 1")
            .replace("sGpuBufferF", "sGpuBufferI"), "Sint", "sGpuBufferI"),
    ] {
        let reflected = reflect(&source);
        assert_eq!(reflected.storage_buffers, BTreeMap::from([(7, (name.to_owned(), scalar))]));
        assert!(!reflected.projection);
        assert!(reflected.textures.is_empty());
        assert!(reflected.samplers.is_empty());
    }
    let variable_readonly = STORAGE.replace("OpMemberDecorate %4 0 NonWritable", "OpDecorate %7 NonWritable");
    assert_eq!(reflect(&variable_readonly).storage_buffers, reflect(STORAGE).storage_buffers);
}

#[test]
fn storage_reflection_rejects_unaudited_layouts() {
    for (from, to) in [
        ("ArrayStride 16", "ArrayStride 32"),
        ("Offset 0", "Offset 16"),
        ("OpMemberDecorate %4 0 NonWritable", ""),
        ("OpTypeVector %1 4", "OpTypeVector %1 3"),
        ("OpTypeFloat 32", "OpTypeFloat 64"),
        ("OpTypeFloat 32", "OpTypeInt 32 0"),
        ("OpTypeRuntimeArray %2", "OpTypeArray %2 %8"),
        ("OpTypeStruct %3", "OpTypeStruct %3 %2"),
        ("OpDecorate %4 Block", "OpDecorate %4 BufferBlock"),
        ("DescriptorSet 0", "DescriptorSet 1"),
    ] {
        let source = STORAGE.replace(from, to);
        assert_ne!(source, STORAGE);
        assert!(std::panic::catch_unwind(|| reflect(&source)).is_err(), "Accepted {} -> {}", from, to);
    }
}

#[test]
fn storage_binding_remap_leaves_other_decorations_unchanged() {
    let words = [
        0x07230203u32, 0x00010300, 0, 8, 0,
        (4 << 16) | 71, 7, 34, 0,
        (4 << 16) | 71, 7, 33, 7,
        (4 << 16) | 71, 3, 6, 16,
    ];
    let bytes: Vec<_> = words.iter().copied().flat_map(u32::to_le_bytes).collect();
    let remapped = remap_bindings(&bytes, &BTreeMap::from([(7, 2)]));
    let mut expected = bytes;
    expected[12 * 4..13 * 4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(remapped, expected);
    assert_eq!(reflect(&STORAGE.replace("Binding 7", "Binding 2")).storage_buffers,
        BTreeMap::from([(2, ("sGpuBufferF".to_owned(), "Float"))]));
}
