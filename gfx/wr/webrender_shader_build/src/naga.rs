/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{fs, io, path::Path};
fn token(value: pp_rs::token::TokenValue) -> String {
    use pp_rs::token::{Punct as P, TokenValue as T};
    match value {
        T::Ident(s) => s,
        T::Integer(i) => format!("{}{}", i.value, if i.signed { "" } else { "u" }),
        T::Float(f) => format!("{:?}", f.value),
        T::Version(v) => format!(
            "\n#version {}\n",
            v.tokens
                .into_iter()
                .map(|t| token(t.value))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        T::Extension(v) => format!(
            "\n#extension {}\n",
            v.tokens
                .into_iter()
                .map(|t| token(t.value))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        T::Pragma(_) => String::new(),
        T::Punct(p) => match p {
            P::AddAssign => "+=",
            P::SubAssign => "-=",
            P::MulAssign => "*=",
            P::DivAssign => "/=",
            P::ModAssign => "%=",
            P::LeftShiftAssign => "<<=",
            P::RightShiftAssign => ">>=",
            P::AndAssign => "&=",
            P::XorAssign => "^=",
            P::OrAssign => "|=",
            P::Increment => "++",
            P::Decrement => "--",
            P::LogicalAnd => "&&",
            P::LogicalOr => "||",
            P::LogicalXor => "^^",
            P::LessEqual => "<=",
            P::GreaterEqual => ">=",
            P::EqualEqual => "==",
            P::NotEqual => "!=",
            P::LeftShift => "<<",
            P::RightShift => ">>",
            P::LeftBrace => "{\n",
            P::RightBrace => "}\n",
            P::LeftParen => "(",
            P::RightParen => ")",
            P::LeftBracket => "[",
            P::RightBracket => "]",
            P::LeftAngle => "<",
            P::RightAngle => ">",
            P::Semicolon => ";\n",
            P::Comma => ",",
            P::Colon => ":",
            P::Dot => ".",
            P::Equal => "=",
            P::Bang => "!",
            P::Minus => "-",
            P::Tilde => "~",
            P::Plus => "+",
            P::Star => "*",
            P::Slash => "/",
            P::Percent => "%",
            P::Pipe => "|",
            P::Caret => "^",
            P::Ampersand => "&",
            P::Question => "?",
        }
        .into(),
    }
}

pub(super) const COMPILER: super::Compiler = super::Compiler {
    name: "naga",
    environment: &[],
    preprocess: |source| preprocess(&fs::read_to_string(source)?),
    compile,
    link: |_, _| Ok(()),
    reflect: |binary| Ok(crate::reflection::reflect_binary(&fs::read(binary)?)),
};

pub(super) fn preprocess(source: &str) -> io::Result<String> {
    let mut output = String::new();
    for item in pp_rs::pp::Preprocessor::new(source) {
        let value = item.map_err(|error| io::Error::other(format!("Preprocessing: {error:?}")))?;
        output.push_str(&token(value.value));
        output.push(' ');
    }
    Ok(output)
}

pub(super) fn compile(path: &Path, output: &Path) -> io::Result<()> {
    let source = fs::read_to_string(path)?;
    let stage = match path.extension().and_then(|v| v.to_str()) {
        Some("vert") => naga::ShaderStage::Vertex,
        Some("frag") => naga::ShaderStage::Fragment,
        _ => return Err(io::Error::other("Unexpected shader stage")),
    };
    let mut frontend = naga::front::glsl::Frontend::default();
    let module = frontend
        .parse(&naga::front::glsl::Options::from(stage), &source)
        .map_err(|error| io::Error::other(error.emit_to_string(&source)))?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| io::Error::other(format!("{path:?}: {error:?}")))?;
    let mut options = naga::back::spv::Options::default();
    options.lang_version = (1, 3);
    options.flags =
        naga::back::spv::WriterFlags::DEBUG | naga::back::spv::WriterFlags::LABEL_VARYINGS;
    options.fake_missing_bindings = false;
    options.force_loop_bounding = false;
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
    let pipeline = naga::back::spv::PipelineOptions {
        shader_stage: stage,
        entry_point: "main".into(),
    };
    let mut writer = naga::back::spv::Writer::new(&options).map_err(io::Error::other)?;
    writer.set_combined_image_samplers(frontend.metadata().combined_samplers.iter().copied());
    let mut words = Vec::new();
    writer
        .write(&module, &info, Some(&pipeline), &None, &mut words)
        .map_err(io::Error::other)?;
    fs::write(
        output,
        words
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
#[path = "naga_tests.rs"]
mod tests;
