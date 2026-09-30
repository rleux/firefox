/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::draw::{DrawBatch, DrawPass};
use super::pipeline::DrawPipeline;
use super::shader::select_draw_shader;
use super::{
    wgt, Buffer, Device, Recording, Samplers, SubmissionQueue, Texture, TextureFilter, TexturePool,
};
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use crate::device::RenderState;
use euclid::default::Transform3D;
use std::rc::Rc;

pub(super) struct TextureBlitter {
    pipeline: Rc<DrawPipeline>,
    quad: Rc<Buffer>,
    samplers: Rc<Samplers>,
}

pub(super) struct TextureBlit<'a> {
    pub source: &'a Rc<Texture>,
    pub target: &'a Rc<Texture>,
    pub source_rect: DeviceIntRect,
    pub target_rect: DeviceIntRect,
    pub filter: TextureFilter,
}

fn supported_format(format: wgt::TextureFormat) -> bool {
    matches!(
        format,
        wgt::TextureFormat::Rgba8Unorm
            | wgt::TextureFormat::Bgra8Unorm
            | wgt::TextureFormat::R8Unorm
    )
}

fn clip_source_axis(
    source: [i32; 2],
    target: [i32; 2],
    source_size: u32,
    target_size: u32,
) -> Option<[f32; 4]> {
    let s = source.map(f64::from);
    let d = target.map(f64::from);
    let ds = s[1] - s[0];
    let dd = d[1] - d[0];
    if ds == 0.0 || dd == 0.0 {
        return None;
    }
    let a = -s[0] / ds;
    let b = (f64::from(source_size) - s[0]) / ds;
    let lo = 0.0f64.max(a.min(b));
    let hi = 1.0f64.min(a.max(b));
    if lo >= hi {
        return None;
    }
    let d0 = d[0] + lo * dd;
    let d1 = d[0] + hi * dd;
    if d0.max(d1) <= 0.0 || d0.min(d1) >= f64::from(target_size) {
        return None;
    }
    // Let rasterization clip the destination without moving the shader's UV clamp edges.
    Some([
        (s[0] + lo * ds) as f32,
        (s[0] + hi * ds) as f32,
        d0 as f32,
        d1 as f32,
    ])
}

impl TextureBlitter {
    pub fn new(
        owner: &Rc<Device>,
        format: wgt::TextureFormat,
        quad: &Rc<Buffer>,
        samplers: &Rc<Samplers>,
    ) -> Result<Self, String> {
        if !supported_format(format) {
            return Err("Unsupported Vulkan blit target format".into());
        }
        if !Rc::ptr_eq(&quad.raw.owner, owner) || !Rc::ptr_eq(samplers.owner(), owner) {
            return Err("Vulkan blit resources belong to another device".into());
        }
        quad.vertex_binding(0, 16)?;
        let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false)?;
        Ok(Self {
            pipeline: DrawPipeline::new(owner, shader, format, false, RenderState::default())?,
            quad: quad.clone(),
            samplers: samplers.clone(),
        })
    }

    pub fn record(
        &self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        scratch: &mut TexturePool,
        blit: TextureBlit<'_>,
    ) -> Result<(), String> {
        let source = blit.source;
        let target = blit.target;
        commands.recording_id(&self.pipeline.raw.owner)?;
        commands.recording_id(&source.raw.owner)?;
        commands.recording_id(&target.raw.owner)?;
        if !supported_format(source.format())
            || target.format() != self.pipeline.format
            || target.target_view().is_none()
            || blit.filter == TextureFilter::Trilinear
        {
            return Err("Unsupported Vulkan texture blit".into());
        }
        if !source.initialized() {
            return Err("Blitting uninitialized Vulkan texture contents".into());
        }
        let ss = source.size();
        let ts = target.size();
        let sr = blit.source_rect;
        let dr = blit.target_rect;
        let Some(x) = clip_source_axis(
            [sr.min.x, sr.max.x],
            [dr.min.x, dr.max.x],
            ss.width,
            ts.width,
        ) else {
            return Ok(());
        };
        let Some(y) = clip_source_axis(
            [sr.min.y, sr.max.y],
            [dr.min.y, dr.max.y],
            ss.height,
            ts.height,
        ) else {
            return Ok(());
        };
        let source = if source.mip_count() > 1 {
            source.mip_view(0)?
        } else {
            source.clone()
        };
        let source = if source.samples_attachment(target) {
            let copy = scratch.acquire(ss.width, ss.height, source.format(), false)?;
            copy.invalidate(commands)?;
            let full =
                DeviceIntRect::from_size(DeviceIntSize::new(ss.width as i32, ss.height as i32));
            copy.copy_from_texture(commands, &source, full, full)?;
            copy
        } else {
            source
        };
        let pass = DrawPass {
            target,
            origin: DeviceIntPoint::zero(),
            depth: None,
            clear_color: None,
            clear_depth: None,
            depth_range: 0.0..1.0,
        };
        let projection = pass.projection(&Transform3D::ortho(
            0.0,
            ts.width as f32,
            0.0,
            ts.height as f32,
            -1.0,
            1.0,
        ));
        let values = [x[2], y[2], x[3], y[3], x[0], y[0], x[1], y[1], 1.0f32];
        let mut bytes = [0; 36];
        for (destination, value) in bytes.chunks_exact_mut(4).zip(values) {
            destination.copy_from_slice(&value.to_ne_bytes());
        }
        let textures = [(source, blit.filter)];
        pass.record_batches(
            commands,
            uploads,
            &self.quad,
            Some(&self.samplers),
            &[DrawBatch {
                pipeline: &self.pipeline,
                projection: Some(&projection),
                textures: &textures,
                buffers: &[],
                instances: &bytes,
                instance_count: 1,
                scissor: DeviceIntRect::from_size(DeviceIntSize::new(
                    ts.width as i32,
                    ts.height as i32,
                )),
            }],
        )
    }

    pub fn generate_mipmaps(
        &self,
        commands: &mut Recording<'_>,
        uploads: &SubmissionQueue,
        scratch: &mut TexturePool,
        texture: &Rc<Texture>,
    ) -> Result<(), String> {
        commands.recording_id(&self.pipeline.raw.owner)?;
        commands.recording_id(&texture.raw.owner)?;
        if texture.format() != self.pipeline.format {
            return Err("Vulkan mipmap format does not match the blit pipeline".into());
        }
        if texture.mip_count() == 1 {
            return Ok(());
        }
        if !texture.initialized() {
            return Err("Generating mipmaps from uninitialized Vulkan texture contents".into());
        }
        let full = |texture: &Texture| {
            let size = texture.size();
            DeviceIntRect::from_size(DeviceIntSize::new(size.width as i32, size.height as i32))
        };
        let mut source = texture.mip_view(0)?;
        for level in 1..texture.mip_count() {
            let target = texture.mip_view(level)?;
            self.record(
                commands,
                uploads,
                scratch,
                TextureBlit {
                    source: &source,
                    target: &target,
                    source_rect: full(&source),
                    target_rect: full(&target),
                    filter: TextureFilter::Linear,
                },
            )?;
            source = target;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "texture_blit_tests.rs"]
mod tests;
