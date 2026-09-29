/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub(crate) fn reflect_binary(bytes: &[u8]) -> Reflection {
    assert!(bytes.len() >= 20 && bytes.len() % 4 == 0);
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    assert_eq!(words[0], 0x07230203);
    let mut types = BTreeMap::<u32, (u32, Vec<u32>)>::new();
    let mut names = BTreeMap::new();
    let mut decorations = BTreeMap::<u32, BTreeMap<u32, Vec<u32>>>::new();
    let mut members = BTreeMap::<(u32, u32), BTreeMap<u32, Vec<u32>>>::new();
    let mut constants = BTreeMap::new();
    let mut variables = Vec::new();
    let mut cursor = 5;
    while cursor < words.len() {
        let count = (words[cursor] >> 16) as usize;
        let op = words[cursor] & 65535;
        assert!(count > 0 && cursor + count <= words.len());
        let a = &words[cursor + 1..cursor + count];
        match op {
            5 => {
                let data: Vec<u8> = a[1..].iter().flat_map(|v| v.to_le_bytes()).collect();
                names.insert(
                    a[0],
                    String::from_utf8(data.split(|&v| v == 0).next().unwrap().to_vec()).unwrap(),
                );
            }
            19..=33 => {
                types.insert(a[0], (op, a[1..].to_vec()));
            }
            43 => {
                constants.insert(a[1], a[2]);
            }
            59 => variables.push((a[0], a[1], a[2])),
            71 => {
                decorations
                    .entry(a[0])
                    .or_default()
                    .insert(a[1], a[2..].to_vec());
            }
            72 => {
                members
                    .entry((a[0], a[1]))
                    .or_default()
                    .insert(a[2], a[3..].to_vec());
            }
            _ => {}
        }
        cursor += count;
    }
    fn shape(
        ty: u32,
        types: &BTreeMap<u32, (u32, Vec<u32>)>,
        constants: &BTreeMap<u32, u32>,
    ) -> (&'static str, u32, u32) {
        let (op, a) = &types[&ty];
        match op {
            22 => {
                assert_eq!(a, &[32]);
                ("Float", 1, 1)
            }
            21 => {
                assert_eq!(a[0], 32);
                assert!(a[1] <= 1);
                (if a[1] == 1 { "Sint" } else { "Uint" }, 1, 1)
            }
            23 => (shape(a[0], types, constants).0, a[1], 1),
            24 | 28 => {
                let (scalar, width, count) = shape(a[0], types, constants);
                (
                    scalar,
                    width,
                    count * if *op == 24 { a[1] } else { constants[&a[1]] },
                )
            }
            _ => panic!("Unsupported numeric interface {op} {a:?}"),
        }
    }
    let mut result = Reflection::default();
    for (pointer, id, storage) in variables {
        let Some(dec) = decorations.get(&id) else {
            continue;
        };
        let (op, args) = &types[&pointer];
        assert_eq!(*op, 32);
        let ty = args[1];
        if let Some(location) = dec.get(&30) {
            let (scalar, components, locations) = shape(ty, &types, &constants);
            let interface = Interface {
                location: location[0],
                scalar,
                components,
                locations,
                flat: dec.contains_key(&14),
                interpolation: [13, 16, 17].iter().enumerate().fold(
                    0,
                    |value, (bit, decoration)| {
                        value | (u8::from(dec.contains_key(decoration)) << bit)
                    },
                ),
                index: dec.get(&32).map_or(0, |v| v[0]),
            };
            let map = match storage {
                1 => &mut result.inputs,
                3 => &mut result.outputs,
                _ => panic!("Invalid interface storage"),
            };
            let name = names[&id].clone();
            let key = if map.contains_key(&name) {
                format!("{name}_location{}", location[0])
            } else {
                name
            };
            assert!(map.insert(key, interface).is_none());
        }
        if let Some(binding) = dec.get(&33) {
            assert_eq!(dec.get(&34).unwrap(), &[0]);
            let binding = binding[0];
            let (kind, args) = &types[&ty];
            match kind {
                30 => {
                    assert!(decorations[&ty].contains_key(&2));
                    let mut inner = ty;
                    let mut readonly = dec.contains_key(&24);
                    let mut layout = None;
                    while types[&inner].0 == 30 {
                        assert_eq!(types[&inner].1.len(), 1);
                        let member = &members[&(inner, 0)];
                        assert_eq!(member.get(&35).unwrap(), &[0]);
                        readonly |= member.contains_key(&24);
                        layout = Some(member);
                        inner = types[&inner].1[0];
                    }
                    if storage == 12 {
                        assert!(readonly);
                        assert_eq!(types[&inner].0, 29);
                        assert_eq!(types[&inner].1.len(), 1);
                        assert_eq!(decorations[&inner].get(&6).unwrap(), &[16]);
                        let (scalar, width, count) = shape(types[&inner].1[0], &types, &constants);
                        assert_eq!((width, count), (4, 1));
                        assert!(matches!(scalar, "Float" | "Sint"));
                        result.storage_buffers.insert(
                            binding,
                            (names[&id].strip_prefix("b_").unwrap().to_owned(), scalar),
                        );
                    } else {
                        assert_eq!((storage, binding), (2, 0));
                        assert_eq!(shape(inner, &types, &constants), ("Float", 4, 4));
                        let layout = layout.unwrap();
                        assert_eq!(layout.get(&7).unwrap(), &[16]);
                        assert!(layout.contains_key(&5));
                        result.projection = true;
                    }
                }
                25 => {
                    assert_eq!(storage, 0);
                    assert_eq!(&args[1..], &[1, 0, 0, 0, 1, 0]);
                    result.textures.insert(
                        binding,
                        (
                            names[&id].strip_prefix("t_").unwrap().to_owned(),
                            shape(args[0], &types, &constants).0,
                        ),
                    );
                }
                26 => {
                    assert_eq!(storage, 0);
                    result.samplers.push(binding);
                }
                _ => panic!("Unsupported descriptor {kind} {args:?}"),
            }
        }
    }
    result
}
