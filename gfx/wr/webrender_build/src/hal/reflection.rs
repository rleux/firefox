/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::collections::BTreeMap;
use std::convert::TryInto;

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
    pub samplers: Vec<u32>,
    pub projection: bool,
}

pub(super) fn reflect(source: &str) -> Reflection {
    let mut names = BTreeMap::new();
    let mut types = BTreeMap::new();
    let mut decorations = BTreeMap::<&str, BTreeMap<&str, &str>>::new();
    let mut members = BTreeMap::<(&str, &str), BTreeMap<&str, &str>>::new();
    let mut variables = Vec::new();
    let mut constants = BTreeMap::new();
    for line in source.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            ["OpName", id, name] => {
                names.insert(*id, name.trim_matches('"'));
            }
            ["OpDecorate", id, key, rest @ ..] => {
                decorations
                    .entry(*id)
                    .or_default()
                    .insert(*key, rest.first().copied().unwrap_or(""));
            }
            ["OpMemberDecorate", id, member, key, rest @ ..] => {
                members
                    .entry((*id, *member))
                    .or_default()
                    .insert(*key, rest.first().copied().unwrap_or(""));
            }
            [id, "=", kind, rest @ ..] if kind.starts_with("OpType") => {
                types.insert(*id, (&kind[6..], rest.to_vec()));
            }
            [id, "=", "OpConstant", _, value] => {
                if let Ok(value) = value.parse::<u32>() {
                    constants.insert(*id, value);
                }
            }
            [id, "=", "OpVariable", pointer, storage, ..] => {
                variables.push((*id, *pointer, *storage));
            }
            _ => {}
        }
    }
    fn shape(
        id: &str,
        types: &BTreeMap<&str, (&str, Vec<&str>)>,
        constants: &BTreeMap<&str, u32>,
    ) -> (&'static str, u32, u32) {
        let (kind, args) = &types[id];
        match *kind {
            "Float" => {
                assert_eq!(args, &["32"]);
                ("Float", 1, 1)
            }
            "Int" => {
                assert_eq!(args[0], "32");
                (if args[1] == "1" { "Sint" } else { "Uint" }, 1, 1)
            }
            "Vector" => {
                let (scalar, _, _) = shape(args[0], types, constants);
                (scalar, args[1].parse().unwrap(), 1)
            }
            "Matrix" => {
                let (scalar, components, _) = shape(args[0], types, constants);
                (scalar, components, args[1].parse().unwrap())
            }
            "Array" => {
                let (scalar, components, locations) = shape(args[0], types, constants);
                (scalar, components, locations * constants[args[1]])
            }
            _ => panic!("Unsupported HAL interface type {} {:?}", kind, args),
        }
    }
    let mut result = Reflection::default();
    for (id, pointer, storage) in variables {
        let Some(decoration) = decorations.get(id) else {
            continue;
        };
        let (kind, args) = &types[pointer];
        assert_eq!(*kind, "Pointer");
        let ty = args[1];
        if let Some(location) = decoration.get("Location") {
            let (scalar, components, locations) = shape(ty, &types, &constants);
            let interface = Interface {
                location: location.parse().unwrap(),
                scalar,
                components,
                locations,
                flat: decoration.contains_key("Flat"),
                interpolation: ["NoPerspective", "Centroid", "Sample"]
                    .iter()
                    .enumerate()
                    .fold(0, |flags, (bit, name)| {
                        flags | (u8::from(decoration.contains_key(name)) << bit)
                    }),
                index: decoration.get("Index").unwrap_or(&"0").parse().unwrap(),
            };
            let map = match storage {
                "Input" => &mut result.inputs,
                "Output" => &mut result.outputs,
                _ => panic!("Invalid interface storage {}", storage),
            };
            assert!(map.insert(names[id].to_owned(), interface).is_none());
        }
        if let Some(binding) = decoration.get("Binding") {
            assert_eq!(decoration.get("DescriptorSet"), Some(&"0"));
            let binding = binding.parse::<u32>().unwrap();
            let (kind, args) = &types[ty];
            match *kind {
                "Struct" => {
                    assert_eq!((binding, storage, args.len()), (0, "Uniform", 1));
                    assert!(decorations[ty].contains_key("Block"));
                    assert_eq!(shape(args[0], &types, &constants), ("Float", 4, 4));
                    let layout = &members[&(ty, "0")];
                    assert_eq!(layout.get("Offset"), Some(&"0"));
                    assert_eq!(layout.get("MatrixStride"), Some(&"16"));
                    assert!(layout.contains_key("ColMajor"));
                    result.projection = true;
                }
                "Image" => {
                    assert_eq!(storage, "UniformConstant");
                    assert_eq!(&args[1..], &["2D", "0", "0", "0", "1", "Unknown"]);
                    let (scalar, _, _) = shape(args[0], &types, &constants);
                    result.textures.insert(
                        binding,
                        (names[id].strip_prefix("t_").unwrap().to_owned(), scalar),
                    );
                }
                "Sampler" => {
                    assert_eq!(storage, "UniformConstant");
                    result.samplers.push(binding);
                }
                _ => panic!("Unsupported HAL descriptor {} {:?}", kind, args),
            }
        }
    }
    result
}
