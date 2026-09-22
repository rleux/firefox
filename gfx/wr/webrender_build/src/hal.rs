/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::shader::{build_shader_strings, shader_source_from_file, ShaderVersion};
use crate::shader_features::get_hal_shader_features;
use regex::Regex;
mod reflection;
#[cfg(feature = "hal-translate")]
pub mod translate;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScalarType {
    Float,
    Sint,
    Uint,
}

#[derive(Debug)]
pub struct TextureBinding {
    pub name: &'static str,
    pub binding: u32,
    pub scalar: ScalarType,
    pub stages: u32,
    pub sampler_stages: u32,
}

#[derive(Debug)]
pub struct StorageBinding {
    pub name: &'static str,
    pub binding: u32,
    pub scalar: ScalarType,
    pub stages: u32,
}

#[derive(Debug)]
pub struct VertexInput {
    pub name: &'static str,
    pub location: u32,
    pub scalar: ScalarType,
    pub components: u32,
}

pub struct ShaderArtifact {
    pub name: &'static str,
    pub features: &'static str,
    pub buffer_tables: bool,
    pub vertex: &'static [u8],
    pub fragment: &'static [u8],
    pub inputs: &'static [VertexInput],
    pub textures: &'static [TextureBinding],
    pub storage_buffers: &'static [StorageBinding],
    pub projection_stages: u32,
    pub digest: u64,
}

const DATA_TABLES: [&str; 6] = [
    "sPrimitiveHeadersF", "sPrimitiveHeadersI", "sGpuBufferF", "sGpuBufferI",
    "sTransformPalette", "sRenderTasks",
];

fn storage_fetches(source: &str) -> String {
    let mut source = source.to_owned();
    for name in DATA_TABLES {
        for (operation, helper) in [("texelFetchOffset", "wr_data_offset"), ("texelFetch", "wr_data_fetch")] {
            source = Regex::new(&format!(r"\b{operation}\s*\(\s*{name}\s*,")).unwrap()
                .replace_all(&source, format!("{helper}_{name}(")).into_owned();
        }
    }
    source
}

fn storage_declaration(name: &str, binding: u32, ty: &str) -> String {
    let element = match ty {
        "sampler2D" => "vec4",
        "isampler2D" => "ivec4",
        _ => panic!("Unsupported HAL storage table {} {}", ty, name),
    };
    let width = crate::MAX_VERTEX_TEXTURE_WIDTH;
    format!(r#"
layout(set = 0, binding = {binding}, std430) readonly buffer WrData_{name} {{ {element} values[]; }} b_{name};
{element} wr_data_fetch_{name}(ivec2 position, int lod) {{
    if (lod != 0 || any(lessThan(position, ivec2(0))) || position.x >= {width}
        || uint(position.y) >= uint(b_{name}.values.length()) / {width}u) {{ return {element}(0); }}
    return b_{name}.values[uint(position.y) * {width}u + uint(position.x)];
}}
{element} wr_data_offset_{name}(ivec2 position, int lod, ivec2 offset) {{
    return wr_data_fetch_{name}(position + offset, lod);
}}
"#)
}

fn run(command: &mut Command) -> io::Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{command:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}

fn scalar_type(ty: &str) -> (&'static str, u32) {
    match ty {
        "float" => ("Float", 1),
        "vec2" => ("Float", 2),
        "vec3" => ("Float", 3),
        "vec4" => ("Float", 4),
        "int" => ("Sint", 1),
        "ivec2" => ("Sint", 2),
        "ivec3" => ("Sint", 3),
        "ivec4" => ("Sint", 4),
        "uint" => ("Uint", 1),
        "uvec2" => ("Uint", 2),
        "uvec3" => ("Uint", 3),
        "uvec4" => ("Uint", 4),
        _ => panic!("Unsupported HAL vertex type {}", ty),
    }
}

fn specialize_samplers(source: &str) -> io::Result<String> {
    let signature =
        Regex::new(r"(?m)^\s*(\w+)\s+(\w+)\s*\(\s*sampler2D\s+(\w+)\s*,([^)]*)\)\s*\{").unwrap();
    let mut output = source.to_owned();
    while let Some(declaration) = signature.captures(&output) {
        let range = declaration.get(0).unwrap();
        let (start, body_start) = (range.start(), range.end());
        let mut depth = 1;
        let mut end = body_start;
        for (index, character) in output[body_start..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = body_start + index + 1;
                break;
            }
        }
        if depth != 0 {
            return Err(io::Error::other("Unterminated HAL sampler helper"));
        }
        let return_type = declaration[1].to_owned();
        let name = declaration[2].to_owned();
        let parameter = declaration[3].to_owned();
        let arguments = declaration[4].to_owned();
        let body = output[body_start..end - 1].to_owned();
        output.replace_range(start..end, "");
        let calls = Regex::new(&format!(r"\b{}\s*\(\s*(\w+)\s*,", name)).unwrap();
        let mut sources = std::collections::BTreeSet::new();
        output = calls
            .replace_all(&output, |call: &regex::Captures| {
                sources.insert(call[1].to_owned());
                format!("{}_{}(", name, &call[1])
            })
            .into_owned();
        if Regex::new(&format!(r"\b{}\s*\(", name))
            .unwrap()
            .is_match(&output)
        {
            return Err(io::Error::other(format!(
                "Unsupported HAL sampler argument to {name}"
            )));
        }
        let parameter = Regex::new(&format!(r"\b{}\b", parameter)).unwrap();
        let mut functions = String::new();
        for sampler in sources {
            let uniform = Regex::new(&format!(r"uniform\s+sampler2D\s+{}\s*;", sampler)).unwrap();
            if !uniform.is_match(&output) {
                return Err(io::Error::other(format!(
                    "HAL sampler helper requires a declared sampler: {sampler}"
                )));
            }
            let body = parameter.replace_all(&body, sampler.as_str());
            functions.push_str(&format!(
                "\n{return_type} {name}_{sampler}({arguments}) {{ {body} }}\n"
            ));
        }
        output.insert_str(start, &functions);
    }
    Ok(output)
}

pub fn build(
    res: &Path,
    out: &Path,
    optimize: impl Fn(String, bool) -> io::Result<String>,
) -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=GLSLANG_VALIDATOR");
    println!("cargo:rerun-if-env-changed=SPIRV_VAL");
    println!("cargo:rerun-if-env-changed=SPIRV_DIS");
    let compiler =
        std::env::var_os("GLSLANG_VALIDATOR").unwrap_or_else(|| "glslangValidator".into());
    let validator = std::env::var_os("SPIRV_VAL").unwrap_or_else(|| "spirv-val".into());
    let disassembler = std::env::var_os("SPIRV_DIS").unwrap_or_else(|| "spirv-dis".into());
    let directory = out.join("hal-shaders");
    fs::create_dir_all(&directory)?;
    let uniforms =
        Regex::new(r"(?m)^\s*uniform\s+(?:(?:highp|mediump|lowp)\s+)?(\w+)\s+(\w+)\s*;").unwrap();
    let interface = Regex::new(
        r"(?m)^\s*((?:(?:flat|smooth|noperspective|centroid|sample)\s+)*)(in|out)\s+(?:(?:highp|mediump|lowp)\s+)?(\w+)\s+(\w+)\s*(\[\s*\d+\s*\])?\s*;",
    )
    .unwrap();
    let mut catalog: Vec<_> = get_hal_shader_features().into_iter().collect();
    for (name, variants) in &mut catalog {
        if matches!(*name, "ps_quad_textured" | "ps_quad_repeat" | "composite" | "cs_scale") {
            let legacy: Vec<_> = variants.iter().filter(|features|
                features.as_str() == "TEXTURE_2D" || (*name == "ps_quad_repeat" && features.is_empty()))
                .map(|features| if features.is_empty() { "HAL_LEGACY_BRILINEAR".into() }
                    else { format!("{features},HAL_LEGACY_BRILINEAR") }).collect();
            variants.extend(legacy);
        }
    }
    catalog.sort_by_key(|entry| entry.0);
    let mut sources = Vec::new();
    let mut textures = BTreeMap::new();
    for (name, variants) in catalog {
        for features in variants {
            let mut defines: Vec<_> = features.split(',').filter(|s| !s.is_empty()).collect();
            if name.starts_with("ps_quad") { defines.push("HAL_AA_GRID"); }
            let (vertex, fragment, _, _) =
                build_shader_strings(ShaderVersion::Gl, &defines, name, &|file| {
                    Cow::Owned(shader_source_from_file(&res.join(format!("{file}.glsl"))))
                });
            let stem = format!("{name}_{}", features.replace(',', "_"));
            let mut stages = Vec::new();
            for (stage, source) in [("vert", vertex), ("frag", fragment)] {
                let source = if features.contains("DITHERING") {
                    source
                } else {
                    optimize(source, stage == "vert")?
                };
                let path = directory.join(format!("{stem}.{stage}"));
                fs::write(&path, source.replacen("#version 150", "#version 450", 1))?;
                let source = run(Command::new(&compiler).arg("-E").arg(&path))?;
                let source = Regex::new(r"\bsampler\b")
                    .unwrap()
                    .replace_all(&source, "wr_sampler")
                    .into_owned();
                let source = specialize_samplers(&source)?;
                for declaration in uniforms.captures_iter(&source) {
                    let ty = &declaration[1];
                    let name = &declaration[2];
                    if name == "uTransform" && ty == "mat4" {
                        continue;
                    }
                    if !matches!(ty, "sampler2D" | "isampler2D" | "usampler2D") {
                        return Err(io::Error::other(format!(
                            "Unsupported HAL uniform {ty} {name}"
                        )));
                    }
                    if let Some(old) = textures.insert(name.to_owned(), ty.to_owned()) {
                        assert_eq!(old, ty, "Conflicting HAL texture type for {name}");
                    }
                }
                stages.push((path, source));
            }
            let buffer_stages = stages.iter().map(|(path, source)| {
                let stage = path.extension().unwrap().to_str().unwrap();
                (directory.join(format!("{stem}_buffers.{stage}")), source.clone())
            }).collect();
            sources.push((name, features.clone(), false, stages));
            sources.push((name, features, true, buffer_stages));
        }
    }
    let bindings: BTreeMap<_, _> = textures
        .iter()
        .enumerate()
        .map(|(index, (name, ty))| (name.as_str(), (1 + index as u32 * 2, ty.as_str())))
        .collect();
    let mut generated = String::from(
        "use webrender_build::hal::{ScalarType, TextureBinding, StorageBinding, VertexInput, ShaderArtifact};\n",
    );
    generated.push_str("pub static SHADERS: &[ShaderArtifact] = &[\n");
    for (name, features, buffer_tables, stages) in sources {
        let mut varying_locations = BTreeMap::new();
        for (index, (_, source)) in stages.iter().enumerate() {
            for declaration in interface.captures_iter(source) {
                if (index == 0 && &declaration[2] == "out")
                    || (index == 1 && &declaration[2] == "in")
                {
                    let columns = if declaration[3].starts_with("mat") {
                        let dimensions: Vec<_> = declaration[3][3..]
                            .split('x')
                            .map(|n| n.parse::<usize>().unwrap())
                            .collect();
                        assert!(
                            dimensions.len() <= 2 && dimensions.iter().all(|n| (2..=4).contains(n))
                        );
                        dimensions[0]
                    } else {
                        scalar_type(&declaration[3]);
                        1
                    };
                    let count = declaration
                        .get(5)
                        .map(|array| {
                            array
                                .as_str()
                                .trim_matches(['[', ']'])
                                .trim()
                                .parse::<usize>()
                                .unwrap()
                        })
                        .unwrap_or(1);
                    assert!(count > 0);
                    let columns = columns.checked_mul(count).unwrap();
                    if let Some(previous) =
                        varying_locations.insert(declaration[4].to_owned(), (0, columns))
                    {
                        assert_eq!(previous.1, columns);
                    }
                }
            }
        }
        let mut next_location = 0;
        for (location, columns) in varying_locations.values_mut() {
            *location = next_location;
            next_location += *columns;
        }
        let mut inputs = Vec::new();
        let mut reflected = Vec::new();
        let mut binaries = Vec::new();
        let mut linked_sources = Vec::new();
        let mut digest = DefaultHasher::new();
        (name, &features, buffer_tables, "vulkan1.1", &textures).hash(&mut digest);
        for (index, (path, source)) in stages.into_iter().enumerate() {
            let legacy = index == 1 && features.split(',').any(|feature| feature == "HAL_LEGACY_BRILINEAR");
            let source = if legacy {
                Regex::new(r"\btexture\s*\(\s*sColor0\s*,").unwrap()
                    .replace_all(&source, "wr_legacy_sample(").into_owned()
            } else { source };
            let source = if buffer_tables { storage_fetches(&source) } else { source };
            let source = uniforms.replace_all(&source, |declaration: &regex::Captures| {
                let name = &declaration[2];
                if name == "uTransform" {
                    return "\nlayout(set = 0, binding = 0, std140) uniform Projection { mat4 uTransform; };\n".to_owned();
                }
                let (binding, ty) = bindings[name];
                if buffer_tables && DATA_TABLES.contains(&name) {
                    return storage_declaration(name, binding, ty);
                }
                let texture_type = ty.replace("sampler", "texture");
                let mut declaration = format!("\nlayout(set = 0, binding = {binding}) uniform {texture_type} t_{name};\nlayout(set = 0, binding = {}) uniform sampler p_{name};\n#define {name} {ty}(t_{name}, p_{name})\n", binding + 1);
                if legacy && name == "sColor0" {
                    declaration.push_str(r#"
vec4 wr_legacy_sample(vec2 uv) {
    vec2 dimensions = vec2(textureSize(sColor0, 0));
    vec2 dx = abs(dFdxCoarse(uv)) * dimensions;
    vec2 dy = abs(dFdyCoarse(uv)) * dimensions;
    float rho = max(max(dx.x, dx.y), max(dy.x, dy.y));
    if (rho <= 1.0) { return textureLod(sColor0, uv, 0.0); }
    uint bits = floatBitsToUint(rho * 1.2374368670764582);
    float level = float(int((bits >> 23u) & 255u) - 127);
    float mantissa = uintBitsToFloat((bits & 8388607u) | 1065353216u);
    float weight = clamp(2.0 * mantissa - 3.0, 0.0, 1.0);
    return textureLod(sColor0, uv, max(0.0, level + weight));
}
"#);
                }
                declaration
            });
            if buffer_tables {
                for table in DATA_TABLES {
                    if Regex::new(&format!(r"\b{table}\b")).unwrap().is_match(&source) {
                        return Err(io::Error::other(format!("Unsupported HAL storage table operation: {table}")));
                    }
                }
            }
            let source = interface.replace_all(&source, |declaration: &regex::Captures| {
                let direction = &declaration[2];
                let ty = &declaration[3];
                let name = &declaration[4];
                let location = if index == 0 && direction == "in" {
                    let location = inputs.len();
                    let (scalar, components) = scalar_type(ty);
                    inputs.push(format!("VertexInput {{ name: {name:?}, location: {location}, scalar: ScalarType::{scalar}, components: {components} }}"));
                    location
                } else if index == 1 && direction == "out" { 0 } else { varying_locations[name].0 };
                format!("\nlayout(location = {location}) {}{direction} {ty} {name}{};\n", &declaration[1], declaration.get(5).map(|v| v.as_str()).unwrap_or(""))
            });
            fs::write(&path, source.as_bytes())?;
            source.hash(&mut digest);
            let binary = path.with_extension(format!(
                "{}.spv",
                path.extension().unwrap().to_str().unwrap()
            ));
            run(Command::new(&compiler)
                .args(["-V", "-Os", "--target-env", "vulkan1.1", "-o"])
                .arg(&binary)
                .arg(&path))?;
            run(Command::new(&validator)
                .args(["--target-env", "vulkan1.1"])
                .arg(&binary))?;
            reflected.push(reflection::reflect(&run(Command::new(&disassembler)
                .arg("--raw-id")
                .arg(&binary))?));
            linked_sources.push(path);
            fs::read(&binary)?.hash(&mut digest);
            binaries.push(fs::canonicalize(binary)?);
        }
        let linked = directory.join("linked-validation.spv");
        run(Command::new(&compiler)
            .args(["-V", "--target-env", "vulkan1.1", "-l", "-o"])
            .arg(&linked)
            .args(&linked_sources))?;
        fs::remove_file(linked)?;
        let mut active_textures = BTreeMap::new();
        let mut active_buffers = BTreeMap::new();
        let mut projection_stages = 0;
        for (index, stage) in reflected.iter().enumerate() {
            if stage.projection {
                projection_stages |= 1 << index;
            }
            for (&binding, (name, scalar)) in &stage.textures {
                assert_eq!(bindings[name.as_str()].0, binding);
                let entry = active_textures
                    .entry(binding)
                    .or_insert((name, *scalar, 0, 0));
                assert_eq!(entry.1, *scalar);
                entry.2 |= 1 << index;
                if stage.samplers.contains(&(binding + 1)) {
                    entry.3 |= 1 << index;
                }
            }
            for (&binding, (name, scalar)) in &stage.storage_buffers {
                assert!(buffer_tables && DATA_TABLES.contains(&name.as_str()));
                assert_eq!(bindings[name.as_str()].0, binding);
                let entry = active_buffers.entry(binding).or_insert((name, *scalar, 0));
                assert_eq!(entry.1, *scalar);
                entry.2 |= 1 << index;
            }
            for binding in &stage.samplers {
                assert!(stage.textures.contains_key(&(binding - 1)));
            }
        }
        for input in reflected[1].inputs.values() {
            let output = reflected[0]
                .outputs
                .values()
                .find(|output| output.location == input.location)
                .unwrap();
            assert_eq!(input, output, "HAL varying mismatch in {name} {features}");
        }
        for output in reflected[1].outputs.values() {
            assert_eq!(
                (
                    output.location,
                    output.scalar,
                    output.components,
                    output.locations
                ),
                (0, "Float", 4, 1)
            );
            assert!(
                output.index == 0
                    || (output.index == 1 && features.contains("DUAL_SOURCE_BLENDING"))
            );
        }
        let mut active_inputs = Vec::new();
        for (input_name, input) in &reflected[0].inputs {
            assert_eq!(
                input.locations, 1,
                "HAL vertex arrays/matrices need explicit vertex descriptors"
            );
            let declaration = format!("VertexInput {{ name: {input_name:?}, location: {}, scalar: ScalarType::{}, components: {} }}", input.location, input.scalar, input.components);
            assert!(
                inputs.contains(&declaration),
                "SPIR-V vertex input differs from generated declaration"
            );
            active_inputs.push(declaration);
        }
        let mut native_bindings = BTreeMap::new();
        if projection_stages != 0 {
            native_bindings.insert(0, 0);
        }
        let active_bindings: std::collections::BTreeSet<_> = active_textures.keys()
            .chain(active_buffers.keys()).copied().collect();
        for binding in active_bindings {
            native_bindings.insert(binding, native_bindings.len() as u32);
            if active_textures.get(&binding).map_or(false, |entry| entry.3 != 0) {
                native_bindings.insert(binding + 1, native_bindings.len() as u32);
            }
        }
        for (index, binary) in binaries.iter().enumerate() {
            let bytes = reflection::remap_bindings(&fs::read(binary)?, &native_bindings);
            fs::write(binary, &bytes)?;
            bytes.hash(&mut digest);
            run(Command::new(&validator)
                .args(["--target-env", "vulkan1.1"])
                .arg(binary))?;
            let native = reflection::reflect(&run(Command::new(&disassembler)
                .arg("--raw-id")
                .arg(binary))?);
            let expected: BTreeMap<_, _> = reflected[index]
                .textures
                .iter()
                .map(|(binding, value)| (native_bindings[binding], value.clone()))
                .collect();
            assert_eq!(native.textures, expected);
            let expected_buffers: BTreeMap<_, _> = reflected[index].storage_buffers.iter()
                .map(|(binding, value)| (native_bindings[binding], value.clone())).collect();
            assert_eq!(native.storage_buffers, expected_buffers);
            assert_eq!(native.projection, reflected[index].projection);
            assert_eq!(native.inputs, reflected[index].inputs);
            assert_eq!(native.outputs, reflected[index].outputs);
            let mut samplers: Vec<_> = reflected[index]
                .samplers
                .iter()
                .map(|binding| native_bindings[binding])
                .collect();
            samplers.sort_unstable();
            let mut actual = native.samplers;
            actual.sort_unstable();
            assert_eq!(actual, samplers);
        }
        let texture_entries: Vec<_> = active_textures.iter().map(|(binding, (name, scalar, stages, sampler_stages))| {
            let binding = native_bindings[binding];
            format!("TextureBinding {{ name: {name:?}, binding: {binding}, scalar: ScalarType::{scalar}, stages: {stages}, sampler_stages: {sampler_stages} }}")
        }).collect();
        let buffer_entries: Vec<_> = active_buffers.iter().map(|(binding, (name, scalar, stages))| {
            let binding = native_bindings[binding];
            format!("StorageBinding {{ name: {name:?}, binding: {binding}, scalar: ScalarType::{scalar}, stages: {stages} }}")
        }).collect();
        generated.push_str(&format!("ShaderArtifact {{ name: {name:?}, features: {features:?}, buffer_tables: {buffer_tables}, vertex: include_bytes!({:?}), fragment: include_bytes!({:?}), inputs: &[{}], textures: &[{}], storage_buffers: &[{}], projection_stages: {projection_stages}, digest: {} }},\n",
            binaries[0].to_str().unwrap(), binaries[1].to_str().unwrap(), active_inputs.join(","), texture_entries.join(","), buffer_entries.join(","), digest.finish()));
    }
    generated.push_str("];\n");
    build_presentation(out)?;
    if std::env::var_os("CARGO_FEATURE_HAL_ANDROID_AHB").is_some() {
        build_native_conversion(out)?;
    }
    fs::write(out.join("hal_shaders.rs"), generated)
}

fn build_presentation(out: &Path) -> io::Result<()> {
    let compiler = std::env::var_os("GLSLANG_VALIDATOR").unwrap_or_else(|| "glslangValidator".into());
    let validator = std::env::var_os("SPIRV_VAL").unwrap_or_else(|| "spirv-val".into());
    let mut digest = DefaultHasher::new();
    let mut binaries = Vec::new();
    for (stage, source) in [("vert", include_str!("hal/present.vert")), ("frag", include_str!("hal/present.frag"))] {
        let path = out.join("hal-shaders").join(format!("hal_present.{stage}"));
        fs::write(&path, source)?;
        let binary = path.with_extension(format!("{stage}.spv"));
        run(Command::new(&compiler).args(["-V", "-Os", "--target-env", "vulkan1.1", "-o"]).arg(&binary).arg(path))?;
        run(Command::new(&validator).args(["--target-env", "vulkan1.1"]).arg(&binary))?;
        fs::read(&binary)?.hash(&mut digest);
        binaries.push(binary);
    }
    fs::write(out.join("hal_present.rs"), format!(
        "pub static PRESENT: ShaderArtifact = ShaderArtifact {{ name: \"hal_present\", features: \"\", buffer_tables: false, vertex: include_bytes!({:?}), fragment: include_bytes!({:?}), inputs: &[], textures: &[], storage_buffers: &[], projection_stages: 0, digest: {} }};\n",
        binaries[0].to_str().unwrap(), binaries[1].to_str().unwrap(), digest.finish()))
}

fn build_native_conversion(out: &Path) -> io::Result<()> {
    let compiler = std::env::var_os("GLSLANG_VALIDATOR").unwrap_or_else(|| "glslangValidator".into());
    let validator = std::env::var_os("SPIRV_VAL").unwrap_or_else(|| "spirv-val".into());
    for (stage, source) in [("vert", include_str!("hal/convert.vert")), ("frag", include_str!("hal/convert.frag"))] {
        let path = out.join("hal-shaders").join(format!("hal_convert.{stage}"));
        fs::write(&path, source)?;
        let binary = path.with_extension(format!("{stage}.spv"));
        run(Command::new(&compiler).args(["-V", "-Os", "--target-env", "vulkan1.1", "-o"]).arg(&binary).arg(path))?;
        run(Command::new(&validator).args(["--target-env", "vulkan1.1"]).arg(binary))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_table_fetches_leave_image_samplers_unchanged() {
        let source = "texelFetch(sGpuBufferF, uv + delta, 0); texelFetchOffset(sPrimitiveHeadersI, uv, 0, ivec2(1, 0)); texelFetch(sColor0, uv, 0);";
        let transformed = storage_fetches(source);
        assert!(transformed.contains("wr_data_fetch_sGpuBufferF( uv + delta, 0)"));
        assert!(transformed.contains("wr_data_offset_sPrimitiveHeadersI( uv, 0, ivec2(1, 0))"));
        assert!(transformed.contains("texelFetch(sColor0, uv, 0)"));
        let declaration = storage_declaration("sGpuBufferI", 7, "isampler2D");
        assert!(declaration.contains("readonly buffer WrData_sGpuBufferI { ivec4 values[]; }"));
        assert!(declaration.contains(&format!("position.x >= {}", crate::MAX_VERTEX_TEXTURE_WIDTH)));
        assert!(declaration.contains("lod != 0") && declaration.contains("lessThan(position, ivec2(0))"));
    }

    #[test]
    fn specializes_only_declared_sampler_arguments() {
        let source = "uniform sampler2D sColor0;\nvec4 sampleInput(sampler2D value, vec2 uv) { return texture(value, uv); }\nvoid main() { vec4 color = sampleInput(sColor0, vec2(0)); }";
        let result = specialize_samplers(source).unwrap();
        assert_eq!(result.matches("sampleInput_sColor0(").count(), 2);
        assert!(!result.contains("sampler2D value"));
        assert!(result.contains("texture(sColor0, uv)"));
        assert!(result.contains("sampleInput_sColor0( vec2(0))"));
        assert!(specialize_samplers(
            &source.replace("sampleInput(sColor0,", "sampleInput(unknown,")
        )
        .is_err());
        assert!(specialize_samplers(
            &source.replace("sampleInput(sColor0,", "sampleInput(selectSampler(),")
        )
        .is_err());
    }
}

pub fn native_backends(os: &str, arch: &str, vulkan: bool, metal: bool) -> (bool, bool) {
    (vulkan && arch != "wasm32" && matches!(os, "linux" | "android" | "windows" | "macos"), metal && os == "macos")
}

pub fn configure_backends() -> (bool, bool) {
    let backends = native_backends(&std::env::var("CARGO_CFG_TARGET_OS").unwrap(),
        &std::env::var("CARGO_CFG_TARGET_ARCH").unwrap(),
        std::env::var_os("CARGO_FEATURE_HAL_VULKAN").is_some(),
        std::env::var_os("CARGO_FEATURE_HAL_METAL").is_some());
    for (name, enabled) in [("wr_hal_vulkan", backends.0), ("wr_hal_metal", backends.1)] {
        println!("cargo:rustc-check-cfg=cfg({name})");
        if enabled { println!("cargo:rustc-cfg={name}"); }
    }
    backends
}

#[cfg(test)]
mod platform_tests {
    #[test]
    fn backend_policy_excludes_unimplemented_targets() {
        for os in ["linux", "windows", "android"] {
            assert_eq!(super::native_backends(os, "x86_64", true, true), (true, false));
        }
        assert_eq!(super::native_backends("macos", "aarch64", true, true), (true, true));
        for os in ["ios", "freebsd", "unknown", "emscripten"] {
            assert_eq!(super::native_backends(os, "wasm32", true, true), (false, false));
        }
        assert_eq!(super::native_backends("freebsd", "x86_64", true, true), (false, false));
        assert_eq!(super::native_backends("linux", "x86_64", false, true), (false, false));
    }
}
