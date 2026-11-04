/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

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

impl RenderDevice {
    pub fn blit_render_target(
        &mut self,
        source: ReadTarget,
        source_rect: FramebufferIntRect,
        target: DrawTarget,
        target_rect: FramebufferIntRect,
        filter: TextureFilter,
    ) -> Result<()> {
        self.flush_pass(StoreOp::Store)?;
        let source = self.textures.read_target(source)?;
        let (target, _, _) = self.textures.draw_target(target)?;
        self.blit_level(
            source,
            0,
            target,
            0,
            source_rect.cast_unit(),
            target_rect.cast_unit(),
            filter,
        )
    }
    pub(super) fn generate_mipmaps(&mut self, image: &Arc<Image>) -> Result<()> {
        for level in 1..image.levels.len() as u32 {
            let rect = |level| {
                let s = image.raw.descriptor().mip_level_size(level).unwrap();
                DeviceIntRect::from_size(DeviceIntSize::new(s.width as i32, s.height as i32))
            };
            self.blit_level(
                image.clone(),
                level - 1,
                image.clone(),
                level,
                rect(level - 1),
                rect(level),
                TextureFilter::Linear,
            )?;
        }
        Ok(())
    }
    fn blit_level(
        &mut self,
        mut source: Arc<Image>,
        mut source_level: u32,
        target: Arc<Image>,
        target_level: u32,
        source_rect: DeviceIntRect,
        target_rect: DeviceIntRect,
        filter: TextureFilter,
    ) -> Result<()> {
        if filter == TextureFilter::Trilinear {
            return Err("Vulkan blits require nearest or linear filtering".into());
        }
        let ss = source
            .raw
            .descriptor()
            .mip_level_size(source_level)
            .ok_or("Invalid source mip")?;
        let ts = target
            .raw
            .descriptor()
            .mip_level_size(target_level)
            .ok_or("Invalid destination mip")?;
        let Some(x) = clip_source_axis(
            [source_rect.min.x, source_rect.max.x],
            [target_rect.min.x, target_rect.max.x],
            ss.width,
            ts.width,
        ) else {
            return Ok(());
        };
        let Some(y) = clip_source_axis(
            [source_rect.min.y, source_rect.max.y],
            [target_rect.min.y, target_rect.max.y],
            ss.height,
            ts.height,
        ) else {
            return Ok(());
        };
        if Arc::ptr_eq(&source.raw, &target.raw) && source_level == target_level {
            let scratch = Image::new(
                &self.owner,
                ss.width,
                ss.height,
                source.format(),
                TextureFilter::Nearest,
                false,
            )?;
            let encoder = self.commands()?;
            encoder.copy_texture_to_texture(
                &wgt::TexelCopyTextureInfo {
                    texture: source.raw.clone(),
                    mip_level: source_level,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &wgt::TexelCopyTextureInfo {
                    texture: scratch.raw.clone(),
                    mip_level: 0,
                    origin: wgt::Origin3d::ZERO,
                    aspect: wgt::TextureAspect::All,
                },
                &ss,
            );
            source = scratch;
            source_level = 0;
        }
        let format = target.format();
        let pipeline = if let Some(pipeline) = self.blit_pipelines.get(&format) {
            pipeline.clone()
        } else {
            let pipeline = DrawPipeline::new(
                &self.owner,
                select_draw_shader("cs_scale", &["TEXTURE_2D"], false)?,
                format,
                false,
                RenderState::default(),
                wgt::PrimitiveTopology::TriangleList,
            )
            .map_err(|e| e.to_string())?;
            self.blit_pipelines.insert(format, pipeline.clone());
            pipeline
        };
        let matrix = projection(&Transform3D::ortho(
            0.,
            ts.width as f32,
            0.,
            ts.height as f32,
            -1.,
            1.,
        ));
        let uniform = self
            .owner
            .create_buffer_with_data(
                "WR blit projection",
                crate::device::as_bytes(&matrix),
                wgt::BufferUsages::UNIFORM,
            )
            .map_err(|e| e.to_string())?;
        let bindings = pipeline
            .create_bindings(
                Some(uniform),
                &[(source.levels[source_level as usize].clone(), filter)],
                &[],
                &self.samplers,
            )
            .map_err(|e| e.to_string())?;
        let instances = self
            .owner
            .create_buffer_with_data(
                "WR blit instance",
                crate::device::as_bytes(&[x[2], y[2], x[3], y[3], x[0], y[0], x[1], y[1], 1.]),
                wgt::BufferUsages::VERTEX,
            )
            .map_err(|e| e.to_string())?;
        let full = DeviceIntRect::from_size(DeviceIntSize::new(ts.width as i32, ts.height as i32));
        let draw = Draw {
            pipeline,
            bindings,
            vertices: self.quad.clone(),
            instances: Some(instances),
            indices: Some((self.quad_indices.clone(), wgt::IndexFormat::Uint16)),
            first: 0,
            count: 6,
            instance_count: 1,
            scissor: full,
            viewport: full,
            depth: 0.0..1.0,
        };
        let previous = self.pass.take();
        self.pass = Some(Pass {
            color: target,
            depth: None,
            level: target_level,
            bounds: full,
            viewport: full,
            scissor: None,
            started: false,
            descriptor: RenderPassDescriptor {
                target: DrawTarget::new_default(full.size(), true),
                render_area: None,
                color_load: LoadOp::Load,
                depth_load: LoadOp::Load,
            },
            pending: vec![draw],
        });
        let result = self.flush_pass(StoreOp::Store);
        self.pass = previous;
        result
    }
    pub fn copy_texture_sub_region(
        &mut self,
        source: &Texture,
        x: usize,
        y: usize,
        target: &Texture,
        dx: usize,
        dy: usize,
        width: usize,
        height: usize,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        self.flush_pass(StoreOp::Store)?;
        let source = self.textures.image(source)?;
        let target = self.textures.image(target)?;
        let size = wgt::Extent3d {
            width: u32::try_from(width).map_err(|e| e.to_string())?,
            height: u32::try_from(height).map_err(|e| e.to_string())?,
            depth_or_array_layers: 1,
        };
        let info = |image: &Arc<Image>, x: usize, y: usize| -> Result<_> {
            Ok(wgt::TexelCopyTextureInfo {
                texture: image.raw.clone(),
                mip_level: 0,
                origin: wgt::Origin3d {
                    x: u32::try_from(x).map_err(|e| e.to_string())?,
                    y: u32::try_from(y).map_err(|e| e.to_string())?,
                    z: 0,
                },
                aspect: wgt::TextureAspect::All,
            })
        };
        let encoder = self.commands()?;
        if Arc::ptr_eq(&source.raw, &target.raw) {
            let scratch = Image::new(
                &self.owner,
                size.width,
                size.height,
                source.format(),
                TextureFilter::Nearest,
                false,
            )?;
            encoder.copy_texture_to_texture(&info(&source, x, y)?, &info(&scratch, 0, 0)?, &size);
            encoder.copy_texture_to_texture(&info(&scratch, 0, 0)?, &info(&target, dx, dy)?, &size);
        } else {
            encoder.copy_texture_to_texture(&info(&source, x, y)?, &info(&target, dx, dy)?, &size);
        }
        self.owner.check().map_err(|e| e.to_string())
    }
}
