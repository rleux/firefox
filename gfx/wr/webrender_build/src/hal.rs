/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::shader::{build_shader_strings, shader_source_from_file, ShaderVersion};
use crate::shader_features::get_hal_shader_features;
use regex::Regex;
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
    pub vertex: &'static [u8],
    pub fragment: &'static [u8],
    pub inputs: &'static [VertexInput],
    pub digest: u64,
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

pub fn build(res: &Path, out: &Path) -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=GLSLANG_VALIDATOR");
    println!("cargo:rerun-if-env-changed=SPIRV_VAL");
    let compiler =
        std::env::var_os("GLSLANG_VALIDATOR").unwrap_or_else(|| "glslangValidator".into());
    let validator = std::env::var_os("SPIRV_VAL").unwrap_or_else(|| "spirv-val".into());
    let directory = out.join("hal-shaders");
    fs::create_dir_all(&directory)?;
    let uniforms =
        Regex::new(r"(?m)^\s*uniform\s+(?:(?:highp|mediump|lowp)\s+)?(\w+)\s+(\w+)\s*;").unwrap();
    let interface = Regex::new(
        r"(?m)^\s*((?:flat\s+)?)(in|out)\s+(?:(?:highp|mediump|lowp)\s+)?(\w+)\s+(\w+)\s*;",
    )
    .unwrap();
    let mut catalog: Vec<_> = get_hal_shader_features().into_iter().collect();
    catalog.sort_by_key(|entry| entry.0);
    let mut sources = Vec::new();
    let mut textures = BTreeMap::new();
    for (name, variants) in catalog {
        for features in variants {
            let defines: Vec<_> = features.split(',').filter(|s| !s.is_empty()).collect();
            let (vertex, fragment, _, _) =
                build_shader_strings(ShaderVersion::Gl, &defines, name, &|file| {
                    Cow::Owned(shader_source_from_file(&res.join(format!("{file}.glsl"))))
                });
            let stem = format!("{name}_{}", features.replace(',', "_"));
            let mut stages = Vec::new();
            for (stage, source) in [("vert", vertex), ("frag", fragment)] {
                let path = directory.join(format!("{stem}.{stage}"));
                fs::write(&path, source.replacen("#version 150", "#version 450", 1))?;
                let source = run(Command::new(&compiler).arg("-E").arg(&path))?;
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
            sources.push((name, features, stages));
        }
    }
    let bindings: BTreeMap<_, _> = textures
        .iter()
        .enumerate()
        .map(|(index, (name, ty))| (name.as_str(), (1 + index as u32 * 2, ty.as_str())))
        .collect();
    let mut generated = String::from(
        "use webrender_build::hal::{ScalarType, TextureBinding, VertexInput, ShaderArtifact};\n",
    );
    generated.push_str("pub static TEXTURE_BINDINGS: &[TextureBinding] = &[\n");
    for (name, (binding, ty)) in &bindings {
        let scalar = match *ty {
            "isampler2D" => "Sint",
            "usampler2D" => "Uint",
            _ => "Float",
        };
        generated.push_str(&format!("TextureBinding {{ name: {name:?}, binding: {binding}, scalar: ScalarType::{scalar} }},\n"));
    }
    generated.push_str("];\npub static SHADERS: &[ShaderArtifact] = &[\n");
    for (name, features, stages) in sources {
        let mut varying_locations = BTreeMap::new();
        for (index, (_, source)) in stages.iter().enumerate() {
            for declaration in interface.captures_iter(source) {
                if (index == 0 && &declaration[2] == "out")
                    || (index == 1 && &declaration[2] == "in")
                {
                    varying_locations.insert(declaration[4].to_owned(), 0);
                }
            }
        }
        for (location, value) in varying_locations.values_mut().enumerate() {
            *value = location;
        }
        let mut inputs = Vec::new();
        let mut binaries = Vec::new();
        let mut linked_sources = Vec::new();
        let mut digest = DefaultHasher::new();
        (name, &features, "vulkan1.1", &textures).hash(&mut digest);
        for (index, (path, source)) in stages.into_iter().enumerate() {
            let source = uniforms.replace_all(&source, |declaration: &regex::Captures| {
                let name = &declaration[2];
                if name == "uTransform" {
                    return "\nlayout(set = 0, binding = 0, std140) uniform Projection { mat4 uTransform; };\n".to_owned();
                }
                let (binding, ty) = bindings[name];
                let texture_type = ty.replace("sampler", "texture");
                format!("\nlayout(set = 0, binding = {binding}) uniform {texture_type} t_{name};\nlayout(set = 0, binding = {}) uniform sampler p_{name};\n#define {name} {ty}(t_{name}, p_{name})\n", binding + 1)
            });
            let source = interface.replace_all(&source, |declaration: &regex::Captures| {
                let direction = &declaration[2];
                let ty = &declaration[3];
                let name = &declaration[4];
                let location = if index == 0 && direction == "in" {
                    let location = inputs.len();
                    let (scalar, components) = scalar_type(ty);
                    inputs.push(format!("VertexInput {{ name: {name:?}, location: {location}, scalar: ScalarType::{scalar}, components: {components} }}"));
                    location
                } else if index == 1 && direction == "out" { 0 } else { varying_locations[name] };
                format!("\nlayout(location = {location}) {}{direction} {ty} {name};\n", &declaration[1])
            });
            fs::write(&path, source.as_bytes())?;
            source.hash(&mut digest);
            let binary = path.with_extension(format!(
                "{}.spv",
                path.extension().unwrap().to_str().unwrap()
            ));
            run(Command::new(&compiler)
                .args(["-V", "--target-env", "vulkan1.1", "-o"])
                .arg(&binary)
                .arg(&path))?;
            run(Command::new(&validator)
                .args(["--target-env", "vulkan1.1"])
                .arg(&binary))?;
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
        generated.push_str(&format!("ShaderArtifact {{ name: {name:?}, features: {features:?}, vertex: include_bytes!({:?}), fragment: include_bytes!({:?}), inputs: &[{}], digest: {} }},\n",
            binaries[0].to_str().unwrap(), binaries[1].to_str().unwrap(), inputs.join(","), digest.finish()));
    }
    generated.push_str("];\n");
    fs::write(out.join("hal_shaders.rs"), generated)
}
