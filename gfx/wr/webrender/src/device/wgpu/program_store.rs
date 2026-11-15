/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::pipeline::DrawPipeline;

use super::shader::{draw_vertex_descriptor, select_draw_shader};
use super::vertex_layout::vertex_layouts;
use super::{core, wgt, Device, Failure};
use crate::device::TextureSlot;
use crate::device::{
    Program, ProgramSourceInfo, ProgramSourceType, RenderState, ShaderError, VertexDescriptor,
};
use crate::internal_types::FastHashMap;
use euclid::default::Transform3D;
use std::collections::hash_map::DefaultHasher;
use std::ffi::CString;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::{
    cell::{Cell, RefCell},
    sync::Arc,
};
use webrender_build::vulkan::ShaderArtifact;

struct Entry {
    // IDs are local; the shared name allocation identifies the owning store entry.
    name: Rc<CString>,
    state: ProgramState,
    pipelines: Vec<(RenderState, Arc<DrawPipeline>)>,
}

#[derive(Default)]
pub(in crate::device::wgpu) struct ProgramStore {
    last_id: u32,
    bound: Option<(u32, RenderState)>,
    entries: FastHashMap<u32, Entry>,
}

impl ProgramStore {
    pub fn create(
        &mut self,
        name: &'static str,
        features: &[&'static str],
    ) -> Result<Program, ShaderError> {
        let error = |message| ShaderError::Compilation(name.into(), message, Vec::new());
        let shader = select_draw_shader(name, features, false).map_err(&error)?;
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
            if actual != expected {
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

    pub fn bind(&mut self, program: &Program, state: RenderState) -> Result<bool, String> {
        self.state(program)?;
        let changed = self.bound != Some((program.id, state));
        self.bound = Some((program.id, state));
        Ok(changed)
    }

    pub fn pipeline(
        &mut self,
        owner: &Arc<Device>,
        format: wgt::TextureFormat,
        depth: bool,
        topology: wgt::PrimitiveTopology,
    ) -> Result<Arc<DrawPipeline>, Failure> {
        let (id, state) = self
            .bound
            .ok_or_else(|| Failure::validation("No Vulkan program is bound"))?;
        let entry = self.entries.get_mut(&id).unwrap();
        if let Some((_, pipeline)) = entry.pipelines.iter().find(|(previous, pipeline)| {
            *previous == state
                && pipeline.format == format
                && pipeline.has_depth == depth
                && pipeline.topology == topology
        }) {
            return Ok(pipeline.clone());
        }
        let pipeline =
            DrawPipeline::new(owner, entry.state.shader, format, depth, state, topology)?;
        entry.pipelines.push((state, pipeline.clone()));
        Ok(pipeline)
    }

    pub fn current(&self) -> Result<&ProgramState, String> {
        self.bound
            .and_then(|(id, _)| self.entries.get(&id))
            .map(|entry| &entry.state)
            .ok_or_else(|| "No Vulkan program is bound".into())
    }

    pub fn unbind(&mut self) {
        self.bound = None;
    }

    pub fn delete(&mut self, program: &mut Program) -> Result<(), String> {
        if program.id == 0 {
            return Ok(());
        }
        self.entry(program)?;
        if self.bound.is_some_and(|(id, _)| id == program.id) {
            self.unbind();
        }
        self.entries.remove(&program.id);
        program.id = 0;
        program.is_initialized = false;
        Ok(())
    }
}

pub(super) struct ProgramState {
    pub shader: &'static ShaderArtifact,
    pub slots: Vec<usize>,
    pub transform: Cell<Transform3D<f32>>,
    pub projection: RefCell<Option<Arc<core::resource::Buffer>>>,
}

impl ProgramState {
    fn new(shader: &'static ShaderArtifact) -> Self {
        Self {
            shader,
            slots: vec![0; shader.textures.len()],
            transform: Cell::new(Transform3D::identity()),
            projection: RefCell::new(None),
        }
    }
    fn shader(&self) -> &'static ShaderArtifact {
        self.shader
    }
    pub fn bind_samplers(&mut self, bindings: &[(&str, TextureSlot)]) {
        for (binding, slot) in self.shader.textures.iter().zip(&mut self.slots) {
            if let Some((_, value)) = bindings.iter().find(|(name, _)| *name == binding.name) {
                *slot = value.0;
            }
        }
    }
    pub fn set_transform(&self, transform: &Transform3D<f32>) {
        if self.transform.replace(*transform) != *transform {
            self.projection.borrow_mut().take();
        }
    }
}
