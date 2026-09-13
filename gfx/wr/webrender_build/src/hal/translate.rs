/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub use naga;
use naga::{
    Binding, Expression, Function, FunctionArgument, FunctionResult, Handle, Span, Statement, Type,
    TypeInner,
};

pub struct ValidatedShader {
    pub module: naga::Module,
    pub info: naga::valid::ModuleInfo,
}

fn column_binding(binding: &Option<Binding>, column: u32) -> Result<Option<Binding>, String> {
    let mut binding = binding.clone();
    match &mut binding {
        Some(Binding::Location { location, .. }) => *location += column,
        _ => return Err("Matrix stage interface needs a location binding".into()),
    }
    Ok(binding)
}

fn column_type(
    types: &mut naga::UniqueArena<Type>,
    ty: Handle<Type>,
) -> Option<(Handle<Type>, u32, u32)> {
    if let TypeInner::Matrix {
        columns,
        rows,
        scalar,
    } = types[ty].inner
    {
        let vector = types.insert(
            Type {
                name: None,
                inner: TypeInner::Vector { size: rows, scalar },
            },
            Span::UNDEFINED,
        );
        Some((
            vector,
            columns as u32,
            (rows as u32).next_power_of_two() * u32::from(scalar.width),
        ))
    } else {
        None
    }
}

fn emit_since(function: &mut Function, start: usize) {
    if function.expressions.len() > start {
        let range = function.expressions.range_from(start);
        function.body.push(Statement::Emit(range), Span::UNDEFINED);
    }
}

// Split matrix varyings at the entry boundary without changing shader arithmetic.
fn flatten_matrix_io(module: &mut naga::Module) -> Result<(), String> {
    let types = &mut module.types;
    for entry in &mut module.entry_points {
        let matrix_arguments = entry
            .function
            .arguments
            .iter()
            .any(|arg| matches!(types[arg.ty].inner, TypeInner::Matrix { .. }));
        let matrix_result = entry.function.result.as_ref().map_or(false, |result| {
            if let TypeInner::Struct { ref members, .. } = types[result.ty].inner {
                members
                    .iter()
                    .any(|member| matches!(types[member.ty].inner, TypeInner::Matrix { .. }))
            } else {
                false
            }
        });
        if !matrix_arguments && !matrix_result {
            continue;
        }
        let mut original = std::mem::take(&mut entry.function);
        let result = original.result.clone();
        let mut wrapper = Function {
            name: Some("wr_stage_interface".into()),
            ..Default::default()
        };
        let mut arguments = Vec::new();
        for argument in &mut original.arguments {
            let mut leaves = Vec::new();
            let columns = column_type(types, argument.ty);
            let count = columns.map_or(1, |(_, count, _)| count);
            for column in 0..count {
                let index = wrapper.arguments.len() as u32;
                wrapper.arguments.push(FunctionArgument {
                    name: argument.name.as_ref().map(|name| {
                        if columns.is_some() {
                            format!("{name}_c{column}")
                        } else {
                            name.clone()
                        }
                    }),
                    ty: columns.map_or(argument.ty, |(ty, _, _)| ty),
                    binding: if columns.is_some() {
                        column_binding(&argument.binding, column)?
                    } else {
                        argument.binding.clone()
                    },
                });
                leaves.push(
                    wrapper
                        .expressions
                        .append(Expression::FunctionArgument(index), Span::UNDEFINED),
                );
            }
            arguments.push((argument.ty, columns.is_some(), leaves));
            argument.binding = None;
        }
        let start = wrapper.expressions.len();
        let arguments = arguments
            .into_iter()
            .map(|(ty, matrix, components)| {
                if matrix {
                    wrapper
                        .expressions
                        .append(Expression::Compose { ty, components }, Span::UNDEFINED)
                } else {
                    components[0]
                }
            })
            .collect();
        emit_since(&mut wrapper, start);
        if let Some(result) = &mut original.result {
            result.binding = None;
        }
        let inner = module.functions.append(original, Span::UNDEFINED);
        let value = result.as_ref().map(|_| {
            wrapper
                .expressions
                .append(Expression::CallResult(inner), Span::UNDEFINED)
        });
        wrapper.body.push(
            Statement::Call {
                function: inner,
                arguments,
                result: value,
            },
            Span::UNDEFINED,
        );
        let value = if matrix_result {
            let result = result.as_ref().unwrap();
            let (old_members, span) = match &types[result.ty].inner {
                TypeInner::Struct { members, span } => (members.clone(), *span),
                _ => unreachable!(),
            };
            let start = wrapper.expressions.len();
            let mut members = Vec::new();
            let mut components = Vec::new();
            for (index, member) in old_members.into_iter().enumerate() {
                let base = wrapper.expressions.append(
                    Expression::AccessIndex {
                        base: value.unwrap(),
                        index: index as u32,
                    },
                    Span::UNDEFINED,
                );
                if let Some((ty, count, stride)) = column_type(types, member.ty) {
                    for column in 0..count {
                        members.push(naga::StructMember {
                            name: member.name.as_ref().map(|name| format!("{name}_c{column}")),
                            ty,
                            binding: column_binding(&member.binding, column)?,
                            offset: member.offset + column * stride,
                        });
                        components.push(wrapper.expressions.append(
                            Expression::AccessIndex {
                                base,
                                index: column,
                            },
                            Span::UNDEFINED,
                        ));
                    }
                } else {
                    members.push(member);
                    components.push(base);
                }
            }
            let ty = types.insert(
                Type {
                    name: Some("WrStageOutput".into()),
                    inner: TypeInner::Struct { members, span },
                },
                Span::UNDEFINED,
            );
            let value = wrapper
                .expressions
                .append(Expression::Compose { ty, components }, Span::UNDEFINED);
            emit_since(&mut wrapper, start);
            wrapper.result = Some(FunctionResult { ty, binding: None });
            Some(value)
        } else {
            wrapper.result = result;
            value
        };
        wrapper
            .body
            .push(Statement::Return { value }, Span::UNDEFINED);
        entry.function = wrapper;
    }
    Ok(())
}

fn lower_fetch_offsets(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::collections::HashMap;
    if data.len() < 20 || data.len() % 4 != 0 {
        return Err("Invalid SPIR-V byte length".into());
    }
    let words: Vec<u32> = data
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect();
    if words[0] != 0x07230203 {
        return Err("Invalid SPIR-V magic".into());
    }
    let mut output = words[..5].to_vec();
    let mut constants = HashMap::new();
    let mut cursor = 5;
    while cursor < words.len() {
        let count = (words[cursor] >> 16) as usize;
        if count == 0 || cursor + count > words.len() {
            return Err("Invalid SPIR-V instruction length".into());
        }
        let mut instruction = words[cursor..cursor + count].to_vec();
        let opcode = instruction[0] & 0xffff;
        if matches!(opcode, 43 | 44) && count >= 3 {
            constants.insert(instruction[2], instruction[1]);
        }
        // Naga 30 drops ConstOffset on OpImageFetch; preserve it as OpIAdd.
        if matches!(opcode, 95 | 98) && count > 5 {
            let operands = instruction[5];
            if operands & !(2 | 8 | 64) != 0 {
                return Err(format!("Unsupported image load operands: {operands:#x}"));
            }
            if operands & 8 != 0 {
                if opcode != 95 || operands & 64 != 0 {
                    return Err("Unsupported offset image load".into());
                }
                let index = 6 + usize::from(operands & 2 != 0);
                if index >= count {
                    return Err("Missing image fetch offset".into());
                }
                let offset = instruction.remove(index);
                let ty = *constants
                    .get(&offset)
                    .ok_or("Image fetch offset is not a constant")?;
                let id = output[3];
                output[3] = id.checked_add(1).ok_or("SPIR-V ID overflow")?;
                output.extend_from_slice(&[(5 << 16) | 128, ty, id, instruction[4], offset]);
                instruction[4] = id;
                instruction[5] &= !8;
                instruction[0] = ((instruction.len() as u32) << 16) | opcode;
            }
        }
        output.extend_from_slice(&instruction);
        cursor += count;
    }
    Ok(output.iter().flat_map(|w| w.to_le_bytes()).collect())
}

pub fn parse_spirv(data: &[u8]) -> Result<ValidatedShader, String> {
    let options = naga::front::spv::Options {
        adjust_coordinate_space: false,
        ..Default::default()
    };
    let mut module = naga::front::spv::parse_u8_slice(&lower_fetch_offsets(data)?, &options)
        .map_err(|error| format!("SPIR-V import: {error:?}"))?;
    flatten_matrix_io(&mut module)?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| format!("Naga validation: {error:?}"))?;
    Ok(ValidatedShader { module, info })
}
