/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn checked(
    source: &str,
    name: &str,
) -> (
    naga::Module,
    naga::valid::ModuleInfo,
    naga::back::spv::Options<'static>,
) {
    let mut frontend = naga::front::glsl::Frontend::default();
    let module = frontend
        .parse(
            &naga::front::glsl::Options::from(naga::ShaderStage::Fragment),
            source,
        )
        .unwrap();
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap();
    let mut options = naga::back::spv::Options::default();
    options.lang_version = (1, 3);
    options.fake_missing_bindings = false;
    for (_, var) in module.global_variables.iter() {
        if let Some(binding) = &var.binding {
            options.binding_map.insert(
                binding.clone(),
                naga::back::spv::BindingInfo {
                    descriptor_set: binding.group,
                    binding: binding.binding,
                    binding_array_size: None,
                },
            );
        }
    }
    let mut writer = naga::back::spv::Writer::new(&options).unwrap();
    writer.set_combined_image_samplers(frontend.metadata().combined_samplers.iter().copied());
    let pipeline = naga::back::spv::PipelineOptions {
        shader_stage: naga::ShaderStage::Fragment,
        entry_point: "main".into(),
    };
    let mut first = Vec::new();
    let mut second = Vec::new();
    writer
        .write(&module, &info, Some(&pipeline), &None, &mut first)
        .unwrap();
    writer
        .write(&module, &info, Some(&pipeline), &None, &mut second)
        .unwrap();
    assert_eq!(first, second, "writer reuse: {name}");
    assert_eq!(first[0], 0x07230203);
    if let Some(directory) = std::env::var_os("WR_SHADER_TEST_OUTPUT") {
        let root = Path::new(&directory);
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join(format!("{name}.spv")),
            first
                .iter()
                .copied()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    }
    (module, info, options)
}

#[test]
fn array_lengths_preserve_pointers_and_constants() {
    for (name,declaration,expression) in [
        ("storage_float","layout(set=0,binding=0,std430) readonly buffer Data {vec4 values[];} data;","data.values.length()"),
        ("storage_signed","layout(set=0,binding=0,std430) readonly buffer Data {uint prefix; ivec4 values[];} data;","data.values.length()"),
        ("storage_uninstanced","layout(set=0,binding=0,std430) readonly buffer Data {vec4 values[];};","values.length()"),
        ("fixed_uniform","layout(set=0,binding=0,std140) uniform Data {vec4 values[3];} data;","data.values.length()"),
        ("constant_array","const vec4 values[2]=vec4[2](vec4(1.0),vec4(2.0));","values.length()"),
        ("returned_array","vec4[2] values(){return vec4[2](vec4(1.0),vec4(2.0));}","values().length()"),
    ] {
        checked(&format!("#version 450\n{declaration}\nlayout(location=0) out vec4 color;\nvoid main(){{color=vec4(float({expression}));}}"),name);
    }
}

#[test]
fn combined_descriptors_and_separate_resources() {
    for (name,declaration,expression) in [
        ("combined_float","layout(set=0,binding=0) uniform sampler2D image;","texture(image,vec2(0.5))"),
        ("combined_signed","layout(set=0,binding=0) uniform isampler2D image;","vec4(texelFetch(image,ivec2(0),0))"),
        ("combined_unsigned","layout(set=0,binding=0) uniform usampler2D image;","vec4(texelFetch(image,ivec2(0),0))"),
        ("combined_fetch","layout(set=0,binding=0) uniform sampler2D image;","texelFetch(image,ivec2(0),0)"),
        ("combined_two","layout(set=1,binding=3) uniform sampler2D image; layout(set=1,binding=8) uniform sampler2D other;","texture(image,vec2(0.5))+texture(other,vec2(0.5))"),
        ("combined_unused","layout(set=0,binding=0) uniform sampler2D image;","vec4(1.0)"),
        ("combined_helper","layout(set=0,binding=0) uniform sampler2D image; vec4 sample_it(){return texture(image,vec2(0.5));}","sample_it()+texture(image,vec2(0.25))"),
        ("combined_mixed","layout(set=0,binding=0) uniform sampler2D image; layout(set=0,binding=2) uniform texture2D tex; layout(set=0,binding=3) uniform sampler samp;","texture(image,vec2(0.5))+texture(sampler2D(tex,samp),vec2(0.5))"),
    ] {
        checked(&format!("#version 450\n{declaration}\nlayout(location=0) out vec4 color;\nvoid main(){{color={expression};}}"),name);
    }
}

#[test]
fn invalid_bindings_and_unsupported_declarations_fail() {
    for source in [
        "#version 450\nvoid f();void f(){f();}void main(){f();}",
        "#version 450\nuniform sampler2D image; void main(){}",
        "#version 450\nlayout(binding=0) uniform sampler2D image[2]; void main(){}",
        "#version 450\nlayout(binding=0) sampler2D image; void main(){}",
        "#version 450\nlayout(binding=0) uniform isampler2D image; layout(location=0) out vec4 color; void main(){color=vec4(texture(image,vec2(0.5)));}",
        "#version 450\nlayout(binding=0) uniform sampler2D image; layout(binding=0) uniform sampler2D other; layout(location=0) out vec4 color; void main(){color=texture(image,vec2(0))+texture(other,vec2(0));}",
    ] {
        let parsed=naga::front::glsl::Frontend::default().parse(&naga::front::glsl::Options::from(naga::ShaderStage::Fragment),source);
        if let Ok(module)=parsed {
            assert!(naga::valid::Validator::new(naga::valid::ValidationFlags::all(),naga::valid::Capabilities::all()).validate(&module).is_err());
        }
    }
}

#[test]
fn frontend_reuse_clears_combined_metadata() {
    let mut frontend = naga::front::glsl::Frontend::default();
    let options = naga::front::glsl::Options::from(naga::ShaderStage::Fragment);
    frontend
        .parse(
            &options,
            "#version 450\nlayout(binding=0) uniform sampler2D image; void main(){}",
        )
        .unwrap();
    assert_eq!(frontend.metadata().combined_samplers.len(), 1);
    frontend
        .parse(&options, "#version 450\nvoid main(){}")
        .unwrap();
    assert!(frontend.metadata().combined_samplers.is_empty());
}

#[test]
fn inconsistent_writer_pair_is_rejected() {
    let (module,info,options)=checked("#version 450\nlayout(binding=0) uniform texture2D a; layout(binding=1) uniform texture2D b; layout(binding=2) uniform sampler s; layout(location=0) out vec4 color; void main(){color=texture(sampler2D(b,s),vec2(0.5));}","separate_control");
    let image = module
        .global_variables
        .iter()
        .find(|(_, v)| v.name.as_deref() == Some("a"))
        .unwrap()
        .0;
    let sampler = module
        .global_variables
        .iter()
        .find(|(_, v)| v.name.as_deref() == Some("s"))
        .unwrap()
        .0;
    let mut writer = naga::back::spv::Writer::new(&options).unwrap();
    writer.set_combined_image_samplers([(image, sampler)]);
    assert!(writer
        .write(&module, &info, None, &None, &mut Vec::new())
        .is_err());
}

#[test]
fn texel_fetch_offsets_are_added_to_spatial_coordinates() {
    for (name, texture, sampler, coordinates, arrayed) in [
        ("offset_float", "texture2D", "sampler2D", "ivec2", false),
        ("offset_signed", "itexture2D", "isampler2D", "ivec2", false),
        (
            "offset_unsigned",
            "utexture2D",
            "usampler2D",
            "ivec2",
            false,
        ),
        (
            "offset_array",
            "texture2DArray",
            "sampler2DArray",
            "ivec3",
            true,
        ),
    ] {
        let source=format!("#version 450\nlayout(set=0,binding=0) uniform {texture} image; layout(set=0,binding=1) uniform sampler s; layout(location=0) flat in {coordinates} position; layout(location=0) out vec4 color; void main(){{color=vec4(texelFetchOffset({sampler}(image,s),position,0,ivec2(2,-1)));}}");
        let (module, _, _) = checked(&source, name);
        let mut found = false;
        for (_, function) in module.functions.iter() {
            for (_, expression) in function.expressions.iter() {
                if let naga::Expression::ImageLoad {
                    coordinate,
                    array_index,
                    ..
                } = *expression
                {
                    assert!(matches!(
                        function.expressions[coordinate],
                        naga::Expression::Binary {
                            op: naga::BinaryOperator::Add,
                            ..
                        }
                    ));
                    assert_eq!(array_index.is_some(), arrayed);
                    found = true;
                }
            }
        }
        assert!(found);
    }
}

#[test]
fn glsl_constructs_used_by_webrender_validate_and_emit() {
    for (name, body) in [
        ("matrix_component", "void main() {mat2 m=mat2(1.0);m[0].x=0.5;color=vec4(m[0],m[1]);}"),
        ("matrix_interface", "layout(location=0) in mat2 basis;void main(){color=vec4(basis[0],basis[1]);}"),
        ("function_order", "vec4 later(float x);void main(){color=later(0.5);}vec4 later(float x){return vec4(x);}"),
        ("precision", "struct S {mediump vec4 v;};vec4 helper(highp vec4 v){return v;}void main(){S s=S(vec4(0.5));color=helper(s.v);}"),
        ("struct_array", "struct S {vec2 v;float x;};void main(){S a[2];a[0]=S(vec2(0.5),1.0);a[1]=a[0];color=vec4(a[1].v,a[1].x,1.0);}"),
        ("component_inout", "void modify(inout float x){x=0.5;}void main(){vec4 v=vec4(0.0);modify(v.x);color=v;}"),
    ] {
        checked(&format!("#version 450\nlayout(location=0,index=0) out vec4 color;\n{body}"),name);
    }
}

#[test]
fn preprocessing_preserves_conditionals_and_rejects_errors() {
    let output = preprocess("#version 450\n#define VALUE 7\n#if VALUE == 7\nconst int chosen = VALUE;\n#else\n#error wrong branch\n#endif\n").unwrap();
    assert!(output.contains("chosen"));
    assert!(!output.contains("VALUE"));
    assert!(preprocess("#error intentional failure\n").is_err());
}
