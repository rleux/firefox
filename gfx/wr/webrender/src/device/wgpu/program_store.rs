/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::draw::DrawPass;
use super::pipeline::DrawPipeline;
use super::program::{ProgramState, ResolvedProgram, ShaderResource};
use super::shader::{draw_vertex_descriptor, select_draw_shader};
use super::vertex_layouts;
use super::Texture;
use crate::device::{
    Program, ProgramSourceInfo, ProgramSourceType, RenderState, ShaderError, VertexDescriptor,
};
use crate::internal_types::FastHashMap;
use std::collections::hash_map::DefaultHasher;
use std::ffi::CString;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

struct Entry {
    // IDs are local; the shared name allocation identifies the owning store entry.
    name: Rc<CString>,
    state: ProgramState,
    pipelines: Vec<(RenderState, Rc<DrawPipeline>)>,
}

#[derive(Default)]
pub(in crate::device::wgpu) struct ProgramStore {
    last_id: u32,
    bound: Option<u32>,
    entries: FastHashMap<u32, Entry>,
    pipeline: Option<(RenderState, Rc<DrawPipeline>)>,
}

impl ProgramStore {
    pub fn create(
        &mut self,
        name: &'static str,
        features: &[&'static str],
        buffer_tables: bool,
    ) -> Result<Program, ShaderError> {
        let error = |message| ShaderError::Compilation(name.into(), message, Vec::new());
        let shader = select_draw_shader(name, features, buffer_tables).map_err(&error)?;
        let id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| error("Vulkan program identifier space exhausted".into()))?;
        let full_name = if shader.features.is_empty() {
            shader.name.to_owned()
        } else {
            format!("{} {}", shader.name, shader.features)
        };
        let full_name = Rc::new(CString::new(full_name).map_err(|e| error(e.to_string()))?);
        let mut digest = DefaultHasher::new();
        digest.write(b"vulkan");
        shader.name.hash(&mut digest);
        shader.features.hash(&mut digest);
        shader.buffer_tables.hash(&mut digest);
        shader.digest.hash(&mut digest);
        self.entries.insert(
            id,
            Entry {
                name: full_name.clone(),
                state: ProgramState::new(shader),
                pipelines: Vec::new(),
            },
        );
        self.last_id = id;
        Ok(Program {
            id,
            u_transform: -1,
            u_texture_size: -1,
            is_initialized: false,
            source_info: ProgramSourceInfo {
                base_filename: name,
                features: features.to_vec(),
                full_name_cstr: full_name,
                source_type: ProgramSourceType::Unoptimized,
                digest: digest.into(),
                #[cfg(feature = "debugger")]
                from_source_override: false,
            },
        })
    }

    fn entry(&self, program: &Program) -> Result<&Entry, String> {
        self.entries
            .get(&program.id)
            .filter(|entry| Rc::ptr_eq(&entry.name, &program.source_info.full_name_cstr))
            .ok_or_else(|| "Invalid or foreign Vulkan program".into())
    }

    pub fn link(
        &mut self,
        program: &mut Program,
        descriptor: &VertexDescriptor,
    ) -> Result<(), ShaderError> {
        let error = |message| {
            ShaderError::Link(
                program.source_info.base_filename.into(),
                message,
                Vec::new(),
            )
        };
        let shader = self.entry(program).map_err(&error)?.state.shader();
        if program.is_initialized {
            return Err(error("Vulkan program is already linked".into()));
        }
        let validate = || -> Result<(), String> {
            let canonical = draw_vertex_descriptor(shader)?;
            let actual = vertex_layouts(descriptor, shader)?;
            let expected = vertex_layouts(canonical, shader)?;
            let vertex_size = |descriptor: &VertexDescriptor| -> u64 {
                descriptor
                    .vertex_attributes
                    .iter()
                    .map(|a| u64::from(a.count) * u64::from(a.kind.size_in_bytes()))
                    .sum()
            };
            if actual != expected || vertex_size(descriptor) != vertex_size(canonical) {
                return Err(
                    "Vulkan program vertex descriptor does not match its shader layout".into(),
                );
            }
            Ok(())
        };
        if let Err(message) = validate() {
            let error = error(message);
            self.delete(program).unwrap();
            return Err(error);
        }
        program.u_transform = if shader.projection_stages != 0 { 0 } else { -1 };
        program.is_initialized = true;
        Ok(())
    }

    pub fn state(&self, program: &Program) -> Result<&ProgramState, String> {
        let entry = self.entry(program)?;
        if !program.is_initialized {
            return Err("Vulkan program is not linked".into());
        }
        Ok(&entry.state)
    }

    pub fn state_mut(&mut self, program: &Program) -> Result<&mut ProgramState, String> {
        let entry = self
            .entries
            .get_mut(&program.id)
            .filter(|entry| Rc::ptr_eq(&entry.name, &program.source_info.full_name_cstr))
            .ok_or_else(|| String::from("Invalid or foreign Vulkan program"))?;
        if !program.is_initialized {
            return Err("Vulkan program is not linked".into());
        }
        Ok(&mut entry.state)
    }

    #[cfg(test)]
    pub fn bind(&mut self, program: &Program) -> Result<bool, String> {
        self.state(program)?;
        let changed = self.bound != Some(program.id);
        if changed {
            self.pipeline = None;
        }
        self.bound = Some(program.id);
        Ok(changed)
    }

    pub fn bind_pipeline(
        &mut self,
        program: &Program,
        state: RenderState,
        pass: &DrawPass<'_>,
    ) -> Result<bool, String> {
        let shader = self.state(program)?.shader();
        if self.bound == Some(program.id) {
            if let Some((previous, pipeline)) = &self.pipeline {
                if *previous == state && Self::compatible(pipeline, pass) {
                    return Ok(false);
                }
            }
        }
        let pipelines = &mut self.entries.get_mut(&program.id).unwrap().pipelines;
        let pipeline = match pipelines.iter().find(|(previous, pipeline)| {
            *previous == state && Self::compatible(pipeline, pass)
        }) {
            Some((_, pipeline)) => pipeline.clone(),
            None => {
                let pipeline = DrawPipeline::new(
                    &pass.target.raw.owner,
                    shader,
                    pass.target.format(),
                    pass.depth.is_some(),
                    state,
                )?;
                pipelines.push((state, pipeline.clone()));
                pipeline
            }
        };
        self.bound = Some(program.id);
        self.pipeline = Some((state, pipeline));
        Ok(true)
    }

    fn compatible(pipeline: &DrawPipeline, pass: &DrawPass<'_>) -> bool {
        Rc::ptr_eq(&pipeline.raw.owner, &pass.target.raw.owner)
            && pipeline.format == pass.target.format()
            && pipeline.has_depth == pass.depth.is_some()
    }

    pub fn resolve_current(
        &self,
        pass: &DrawPass<'_>,
        slot_resource: impl FnMut(usize) -> Option<Option<ShaderResource>>,
        fallback: Option<&Rc<Texture>>,
    ) -> Result<ResolvedProgram, String> {
        let pipeline = &self
            .pipeline
            .as_ref()
            .ok_or("No Vulkan pipeline is bound")?
            .1;
        if !Self::compatible(pipeline, pass) {
            return Err("Bound Vulkan pipeline does not match the render target".into());
        }
        self.current()?.resolve(pipeline, pass, slot_resource, fallback)
    }

    pub fn current(&self) -> Result<&ProgramState, String> {
        self.bound
            .and_then(|id| self.entries.get(&id))
            .map(|entry| &entry.state)
            .ok_or_else(|| "No Vulkan program is bound".into())
    }

    pub fn unbind(&mut self) {
        self.bound = None;
        self.pipeline = None;
    }

    pub fn delete(&mut self, program: &mut Program) -> Result<(), String> {
        if program.id == 0 {
            return Ok(());
        }
        self.entry(program)?;
        if self.bound == Some(program.id) {
            self.unbind();
        }
        self.entries.remove(&program.id);
        program.id = 0;
        program.is_initialized = false;
        Ok(())
    }
}

#[cfg(test)]
#[path = "program_store_tests.rs"]
mod tests;
