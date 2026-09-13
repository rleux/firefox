/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use serde_json::{json, Value};
use std::{env, fs, path::Path};

fn interface(module: &naga::Module) -> Vec<String> {
    fn leaves(
        module: &naga::Module,
        ty: naga::Handle<naga::Type>,
        binding: &Option<naga::Binding>,
        prefix: &str,
        output: &mut Vec<String>,
    ) {
        match &module.types[ty].inner {
            naga::TypeInner::Struct { members, .. } => {
                for member in members {
                    leaves(module, member.ty, &member.binding, prefix, output);
                }
            }
            naga::TypeInner::Matrix {
                columns,
                rows,
                scalar,
            } => {
                for column in 0..*columns as u32 {
                    let mut binding = binding.clone();
                    if let Some(naga::Binding::Location { location, .. }) = &mut binding {
                        *location += column;
                    }
                    output.push(format!(
                        "{prefix} {binding:?} {:?}",
                        naga::TypeInner::Vector {
                            size: *rows,
                            scalar: *scalar
                        }
                    ));
                }
            }
            inner => output.push(format!("{prefix} {binding:?} {inner:?}")),
        }
    }
    let mut output = Vec::new();
    for entry in &module.entry_points {
        for argument in &entry.function.arguments {
            leaves(
                module,
                argument.ty,
                &argument.binding,
                &format!("{:?} input", entry.stage),
                &mut output,
            );
        }
        if let Some(result) = &entry.function.result {
            leaves(
                module,
                result.ty,
                &result.binding,
                &format!("{:?} output", entry.stage),
                &mut output,
            );
        }
    }
    output.sort();
    output
}

fn native_outputs(shader: &webrender_build::hal::translate::ValidatedShader, path: &Path) -> Value {
    let mut msl = naga::back::msl::Options {
        lang_version: (2, 4),
        fake_missing_bindings: false,
        ..Default::default()
    };
    let mut hlsl = naga::back::hlsl::Options {
        shader_model: naga::back::hlsl::ShaderModel::V6_0,
        fake_missing_bindings: false,
        ..Default::default()
    };
    let mut resources = naga::back::msl::EntryPointResources::default();
    let mut counts = [0u8; 3];
    for (_, global) in shader.module.global_variables.iter() {
        let Some(binding) = global.binding else {
            continue;
        };
        let mut target = naga::back::msl::BindTarget::default();
        let namespace = match shader.module.types[global.ty].inner {
            naga::TypeInner::Image { .. } => {
                target.texture = Some(counts[1]);
                1
            }
            naga::TypeInner::Sampler { .. } => {
                target.sampler = Some(naga::back::msl::BindSamplerTarget::Resource(counts[2]));
                2
            }
            _ => {
                target.buffer = Some(counts[0]);
                0
            }
        };
        hlsl.binding_map.insert(
            binding,
            naga::back::hlsl::BindTarget {
                register: u32::from(counts[namespace]),
                ..Default::default()
            },
        );
        counts[namespace] += 1;
        resources.resources.insert(binding, target);
    }
    hlsl.sampler_buffer_binding_map.insert(
        naga::back::hlsl::SamplerIndexBufferKey { group: 0 },
        naga::back::hlsl::BindTarget {
            register: 0,
            space: 2,
            ..Default::default()
        },
    );
    let entry = &shader.module.entry_points[0];
    msl.per_entry_point_map
        .insert(entry.name.clone(), resources);
    let msl_options = naga::back::msl::PipelineOptions {
        entry_point: Some((entry.stage, entry.name.clone())),
        ..Default::default()
    };
    let hlsl_options = naga::back::hlsl::PipelineOptions {
        entry_point: Some((entry.stage, entry.name.clone())),
    };
    let metal =
        match naga::back::msl::write_string(&shader.module, &shader.info, &msl, &msl_options) {
            Ok((source, info)) if info.entry_point_names.iter().all(Result::is_ok) => {
                fs::write(path.with_extension("metal"), source).unwrap();
                json!({"emitted": true})
            }
            Ok((_, info)) => json!({"error": format!("{:?}", info.entry_point_names)}),
            Err(error) => json!({"error": format!("{error:?}")}),
        };
    let mut source = String::new();
    let result = naga::back::hlsl::Writer::new(&mut source, &hlsl, &hlsl_options).write(
        &shader.module,
        &shader.info,
        None,
    );
    let dx12 = match result {
        Ok(info) if info.entry_point_names.iter().all(Result::is_ok) => {
            fs::write(path.with_extension("hlsl"), source).unwrap();
            json!({"emitted": true})
        }
        Ok(info) => json!({"error": format!("{:?}", info.entry_point_names)}),
        Err(error) => json!({"error": format!("{error:?}")}),
    };
    json!({"msl": metal, "hlsl": dx12, "binding_mapping": "explicit diagnostic slots; HAL owns native runtime remapping"})
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let catalog: Value = serde_json::from_slice(&fs::read(&args[1]).unwrap()).unwrap();
    let mut results = Vec::new();
    for (index, entry) in catalog.as_array().unwrap().iter().enumerate() {
        let data = fs::read(entry["path"].as_str().unwrap()).unwrap();
        let mut result = entry.clone();
        match webrender_build::hal::translate::parse_spirv(&data) {
            Err(error) => result["translation_error"] = json!(error),
            Ok(shader) => {
                result["validated"] = json!(true);
                let original = naga::front::spv::parse_u8_slice(
                    &data,
                    &naga::front::spv::Options {
                        adjust_coordinate_space: false,
                        ..Default::default()
                    },
                )
                .unwrap();
                let before = interface(&original);
                let after = interface(&shader.module);
                result["interface_preserved"] = json!(before == after);
                result["interface"] = json!(after);
                if before != after {
                    result["original_interface"] = json!(before);
                }
                result["entry_points"] = json!(shader
                    .module
                    .entry_points
                    .iter()
                    .map(|ep| { json!({"name": ep.name, "stage": format!("{:?}", ep.stage)}) })
                    .collect::<Vec<_>>());
                if let Some(directory) = args.get(3) {
                    fs::create_dir_all(directory).unwrap();
                    result["native_outputs"] = native_outputs(
                        &shader,
                        &Path::new(directory).join(format!("stage-{index}")),
                    );
                }
            }
        }
        results.push(result);
    }
    let failures = results
        .iter()
        .filter(|r| {
            r["validated"] != true
                || r["interface_preserved"] != true
                || (r.get("native_outputs").is_some()
                    && (r["native_outputs"]["msl"]["emitted"] != true
                        || r["native_outputs"]["hlsl"]["emitted"] != true))
        })
        .count();
    fs::write(
        Path::new(&args[2]),
        serde_json::to_vec_pretty(&results).unwrap(),
    )
    .unwrap();
    println!(
        "Naga catalog: {} modules, {failures} failures",
        results.len()
    );
    if failures != 0 {
        std::process::exit(1);
    }
}
