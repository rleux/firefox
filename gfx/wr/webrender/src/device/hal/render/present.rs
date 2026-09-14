/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::hal::surface::{SurfaceSetup, SurfaceState, PresentationMethod};
use std::borrow::Borrow;

impl<A: BackendApi> FrameRenderer<A> {
    pub fn attach_surface(
        &mut self,
        setup: SurfaceSetup<A>,
        size: [u32; 2],
        options: SurfaceOptions,
    ) -> Result<()> {
        let mut surface = SurfaceState::new(&self.owner, setup, options)?;
        surface.configure(size)?;
        self.surface = Some(surface);
        Ok(())
    }

    pub fn surface_info(&self) -> Option<SurfaceInfo> {
        self.surface.as_ref().map(|surface| surface.info.clone())
    }

    pub fn resize_surface(&mut self, size: [u32; 2]) -> Result<()> {
        if self.is_failed() {
            return Err("Cannot configure a failed renderer".into());
        }
        self.discard_surface()?;
        self.submissions.wait()?;
        self.surface
            .as_mut()
            .ok_or("Renderer has no window surface")?
            .configure(size)
    }

    pub fn acquire_surface(&mut self) -> Result<PresentationStatus> {
        if self.is_failed() {
            return Err("Cannot acquire on a failed renderer".into());
        }
        let surface = self
            .surface
            .as_mut()
            .ok_or("Renderer has no window surface")?;
        if surface.dirty {
            self.submissions.wait()?;
            surface.configure(surface.info.size)?;
        }
        let fence = self.submissions.fence()?;
        let result = surface.acquire(&fence);
        if result.is_err() && surface.acquired.is_none() {
            self.failed.set(true);
        }
        result
    }

    pub fn discard_surface(&mut self) -> Result<()> {
        if self.is_failed() {
            return Err("Cannot discard on a failed renderer; destroy it".into());
        }
        let surface = self
            .surface
            .as_mut()
            .ok_or("Renderer has no window surface")?;
        if let Some(acquired) = &surface.acquired {
            drop(self.submissions.recording()?);
            self.submissions.submit_surfaces(&[&acquired.texture])?;
            self.submissions.wait()?;
            surface.discard();
        }
        Ok(())
    }

    pub fn present_output(&mut self, output: &RenderedFrame<A>) -> Result<PresentationStatus> {
        if self.is_failed() {
            return Err("Cannot present a failed renderer".into());
        }
        if output.texture.is_none() {
            self.discard_surface()?;
            return Ok(PresentationStatus::Suspended);
        }
        if self
            .surface
            .as_ref()
            .ok_or("Renderer has no window surface")?
            .acquired
            .is_none()
        {
            let status = self.acquire_surface()?;
            if status != PresentationStatus::Acquired {
                return Ok(status);
            }
        }
        let surface = self.surface.as_ref().unwrap();
        let config = surface.config.as_ref().unwrap();
        let format = config.format;
        let size = [config.extent.width, config.extent.height];
        let method = surface.presentation;
        let source = output.texture.as_ref().unwrap();
        let draw = if method == PresentationMethod::Draw {
            let region = PresentationRegion::new(
                [0, 0, output.size[0], output.size[1]],
                [0, 0, size[0], size[1]],
                [source.size.width, source.size.height],
                size,
            )?;
            Some((self.presentation_pipeline(format)?, region))
        } else {
            None
        };
        let surface = self.surface.as_ref().unwrap();
        let acquired = surface.acquired.as_ref().unwrap();
        let target: &A::Texture = acquired.texture.borrow();
        self.failed.set(true);
        if let Some((pipeline, region)) = draw {
            self.record_present_draw(
                pipeline,
                source,
                target,
                format,
                size,
                region,
                wgt::TextureUses::UNINITIALIZED,
                wgt::TextureUses::PRESENT,
            )?;
        } else {
            let mut commands = self.submissions.recording()?;
            let previous = source.current_usage();
            source.transition(&mut commands, wgt::TextureUses::COPY_SRC);
            let range = presentation_range();
            unsafe {
                commands
                    .encoder()
                    .transition_textures(std::iter::once(hal::TextureBarrier {
                        texture: target,
                        range: range.clone(),
                        usage: hal::StateTransition {
                            from: wgt::TextureUses::UNINITIALIZED,
                            to: wgt::TextureUses::COPY_DST,
                        },
                    }));
                A::record_presentation_blit(
                    &self.owner.open.device,
                    commands.encoder(),
                    &source.raw,
                    target,
                    output.size,
                    size,
                )?;
                commands
                    .encoder()
                    .transition_textures(std::iter::once(hal::TextureBarrier {
                        texture: target,
                        range,
                        usage: hal::StateTransition {
                            from: wgt::TextureUses::COPY_DST,
                            to: wgt::TextureUses::PRESENT,
                        },
                    }));
            }
            source.transition(&mut commands, previous);
        }
        self.submissions.submit_surfaces(&[&acquired.texture])?;
        let status = self.surface.as_mut().unwrap().present()?;
        self.failed.set(false);
        Ok(status)
    }
}

fn presentation_range() -> wgt::ImageSubresourceRange {
    wgt::ImageSubresourceRange {
        mip_level_count: Some(1),
        array_layer_count: Some(1),
        ..Default::default()
    }
}

struct PresentationRegion {
    source: [u32; 4],
    target: [u32; 4],
}

impl PresentationRegion {
    fn new(
        source: [u32; 4],
        target: [u32; 4],
        source_size: [u32; 2],
        target_size: [u32; 2],
    ) -> Result<Self> {
        for (rect, size) in [(source, source_size), (target, target_size)] {
            for axis in 0..2 {
                if rect[axis + 2] == 0
                    || rect[axis]
                        .checked_add(rect[axis + 2])
                        .map_or(true, |end| end > size[axis])
                {
                    return Err("Presentation rectangle is empty or outside the texture".into());
                }
            }
        }
        for axis in 2..4 {
            target[axis]
                .checked_mul(2)
                .and_then(|v| v.checked_sub(1))
                .and_then(|v| v.checked_mul(source[axis]))
                .ok_or("Presentation scaling exceeds the integer coordinate range")?;
        }
        Ok(Self { source, target })
    }
}

pub(super) struct PresentationPipeline<A: hal::Api> {
    pipeline: Owned<A, A::RenderPipeline>,
    layout: Owned<A, A::PipelineLayout>,
    bindings: Owned<A, A::BindGroupLayout>,
}

impl<A: BackendApi> FrameRenderer<A> {
    fn presentation_pipeline(
        &mut self,
        format: wgt::TextureFormat,
    ) -> Result<Rc<PresentationPipeline<A>>> {
        if let Some(pipeline) = self.presentation_pipelines.get(&format) {
            return Ok(pipeline.clone());
        }
        if !matches!(
            format,
            wgt::TextureFormat::Rgba8Unorm
                | wgt::TextureFormat::Bgra8Unorm
                | wgt::TextureFormat::Rgba8UnormSrgb
                | wgt::TextureFormat::Bgra8UnormSrgb
        ) {
            return Err("Unsupported presentation attachment format".into());
        }
        let native = &self.owner.open.device;
        let bindings = Owned::new(
            &self.owner,
            unsafe {
                native.create_bind_group_layout(&hal::BindGroupLayoutDescriptor {
                    label: Some("WR presentation bindings"),
                    flags: hal::BindGroupLayoutFlags::empty(),
                    entries: &[
                        wgt::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgt::ShaderStages::FRAGMENT,
                            ty: wgt::BindingType::Buffer {
                                ty: wgt::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: std::num::NonZeroU64::new(48),
                            },
                            count: None,
                        },
                        wgt::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgt::ShaderStages::FRAGMENT,
                            ty: wgt::BindingType::Texture {
                                sample_type: wgt::TextureSampleType::Float { filterable: false },
                                view_dimension: wgt::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                    ],
                })
            }
            .map_err(|e| format!("Creating presentation bindings: {e:?}"))?,
            A::Device::destroy_bind_group_layout,
        );
        let layout = Owned::new(
            &self.owner,
            unsafe {
                native.create_pipeline_layout(&hal::PipelineLayoutDescriptor {
                    label: Some("WR presentation layout"),
                    flags: hal::PipelineLayoutFlags::empty(),
                    bind_group_layouts: &[Some(&bindings)],
                    immediate_size: 0,
                })
            }
            .map_err(|e| format!("Creating presentation layout: {e:?}"))?,
            A::Device::destroy_pipeline_layout,
        );
        let mut cache = self.shader_cache.borrow_mut();
        let module = |fragment, cache: &mut ShaderCache| -> Result<_> {
            Ok(Owned::new(
                &self.owner,
                A::create_shader_module(
                    native,
                    &shaders::PRESENT,
                    fragment,
                    self.shader_input,
                    cache,
                )?,
                A::Device::destroy_shader_module,
            ))
        };
        let vs = module(false, &mut cache)?;
        let fs = module(true, &mut cache)?;
        let constants = Default::default();
        let stage = |module| hal::ProgrammableStage {
            module,
            entry_point: "main",
            constants: &constants,
            zero_initialize_workgroup_memory: false,
        };
        let pipeline = Owned::new(
            &self.owner,
            unsafe {
                native.create_render_pipeline(&hal::RenderPipelineDescriptor {
                    label: Some("WR presentation"),
                    layout: &layout,
                    vertex_processor: hal::VertexProcessor::Standard {
                        vertex_buffers: &[],
                        vertex_stage: stage(&*vs),
                    },
                    fragment_stage: Some(stage(&*fs)),
                    primitive: wgt::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgt::MultisampleState::default(),
                    color_targets: &[Some(wgt::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgt::ColorWrites::ALL,
                    })],
                    multiview_mask: None,
                    cache: None,
                })
            }
            .map_err(|e| format!("Creating presentation pipeline: {e:?}"))?,
            A::Device::destroy_render_pipeline,
        );
        let pipeline = Rc::new(PresentationPipeline {
            pipeline,
            layout,
            bindings,
        });
        self.presentation_pipelines.insert(format, pipeline.clone());
        Ok(pipeline)
    }

    fn record_present_draw(
        &self,
        pipeline: Rc<PresentationPipeline<A>>,
        source: &Rc<Texture<A>>,
        target: &A::Texture,
        format: wgt::TextureFormat,
        size: [u32; 2],
        region: PresentationRegion,
        from: wgt::TextureUses,
        to: wgt::TextureUses,
    ) -> Result<()> {
        if !matches!(
            source.format,
            wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
        ) || !source.sample_initialized()
        {
            return Err("Presentation requires initialized UNORM color input".into());
        }
        let mut parameters = Vec::with_capacity(48);
        for word in region
            .source
            .iter()
            .chain(region.target.iter())
            .copied()
            .chain([u32::from(format.is_srgb()), 0, 0, 0])
        {
            parameters.extend_from_slice(&word.to_ne_bytes());
        }
        let uniform = self
            .submissions
            .upload(&parameters, wgt::BufferUses::UNIFORM)?;
        let native = &self.owner.open.device;
        let target_view = Rc::new(Owned::new(
            &self.owner,
            unsafe {
                native.create_texture_view(
                    target,
                    &hal::TextureViewDescriptor {
                        label: Some("WR presentation target"),
                        format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage: wgt::TextureUses::COLOR_TARGET,
                        range: presentation_range(),
                    },
                )
            }
            .map_err(|e| format!("Creating presentation view: {e:?}"))?,
            A::Device::destroy_texture_view,
        ));
        let bindings = Rc::new(Owned::new(
            &self.owner,
            unsafe {
                native.create_bind_group(&hal::BindGroupDescriptor {
                    label: Some("WR presentation resources"),
                    layout: &pipeline.bindings,
                    entries: &[
                        hal::BindGroupEntry {
                            binding: 0,
                            resource_index: 0,
                            count: 1,
                        },
                        hal::BindGroupEntry {
                            binding: 1,
                            resource_index: 0,
                            count: 1,
                        },
                    ],
                    buffers: &[uniform.binding()],
                    textures: &[hal::TextureBinding {
                        view: &source.view,
                        usage: wgt::TextureUses::RESOURCE,
                    }],
                    samplers: &[],
                    acceleration_structures: &[],
                    external_textures: &[],
                })
            }
            .map_err(|e| format!("Creating presentation resources: {e:?}"))?,
            A::Device::destroy_bind_group,
        ));
        let mut commands = self.submissions.recording()?;
        commands.keep(bindings.clone());
        commands.keep(target_view.clone());
        commands.keep(pipeline.clone());
        let previous = source.current_usage();
        source.transition(&mut commands, wgt::TextureUses::RESOURCE);
        uniform.transition(&mut commands, wgt::BufferUses::UNIFORM);
        unsafe {
            commands
                .encoder()
                .transition_textures(std::iter::once(hal::TextureBarrier {
                    texture: target,
                    range: presentation_range(),
                    usage: hal::StateTransition {
                        from,
                        to: wgt::TextureUses::COLOR_TARGET,
                    },
                }));
            commands
                .encoder()
                .begin_render_pass(&hal::RenderPassDescriptor {
                    label: Some("WR presentation"),
                    extent: wgt::Extent3d {
                        width: size[0],
                        height: size[1],
                        depth_or_array_layers: 1,
                    },
                    sample_count: 1,
                    color_attachments: &[Some(hal::ColorAttachment {
                        target: hal::Attachment {
                            view: &target_view,
                            usage: wgt::TextureUses::COLOR_TARGET,
                        },
                        depth_slice: None,
                        resolve_target: None,
                        ops: (if from == wgt::TextureUses::UNINITIALIZED {
                            hal::AttachmentOps::LOAD_CLEAR
                        } else {
                            hal::AttachmentOps::LOAD
                        }) | hal::AttachmentOps::STORE,
                        clear_value: wgt::Color::TRANSPARENT,
                    })],
                    depth_stencil_attachment: None,
                    multiview_mask: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .map_err(|e| format!("Beginning presentation: {e:?}"))?;
            commands.encoder().set_viewport(
                &hal::Rect {
                    x: 0.0,
                    y: 0.0,
                    w: size[0] as f32,
                    h: size[1] as f32,
                },
                0.0..1.0,
            );
            commands.encoder().set_scissor_rect(&hal::Rect {
                x: region.target[0],
                y: region.target[1],
                w: region.target[2],
                h: region.target[3],
            });
            commands.encoder().set_render_pipeline(&pipeline.pipeline);
            commands
                .encoder()
                .set_bind_group(&pipeline.layout, 0, &bindings, &[]);
            commands.encoder().draw(0, 3, 0, 1);
            commands.encoder().end_render_pass();
            commands
                .encoder()
                .transition_textures(std::iter::once(hal::TextureBarrier {
                    texture: target,
                    range: presentation_range(),
                    usage: hal::StateTransition {
                        from: wgt::TextureUses::COLOR_TARGET,
                        to,
                    },
                }));
        }
        source.transition(&mut commands, previous);
        Ok(())
    }
}

#[cfg(all(test, feature = "hal-vulkan"))]
mod tests {
    use super::*;

    #[test]
    fn presentation_regions_reject_invalid_extents() {
        assert!(PresentationRegion::new([0, 0, 0, 1], [0, 0, 1, 1], [1, 1], [1, 1]).is_err());
        assert!(PresentationRegion::new([1, 0, 1, 1], [0, 0, 1, 1], [1, 1], [1, 1]).is_err());
        assert!(
            PresentationRegion::new([0, 0, 1, 1], [u32::MAX, 0, 1, 1], [1, 1], [1, 1]).is_err()
        );
        assert!(PresentationRegion::new(
            [0, 0, 65536, 1],
            [0, 0, 65536, 1],
            [65536, 1],
            [65536, 1]
        )
        .is_err());
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn presentation_draw_preserves_encoding_crop_and_scaling() {
        let device = create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap();
        let mut renderer = FrameRenderer::new(device).unwrap();
        let size = DeviceIntSize::new(17, 13);
        let colors: Vec<[u8; 4]> = (0..size.height)
            .flat_map(|y| {
                (0..size.width).map(move |x| {
                    let alpha = ((x * 17 + y * 23) % 256) as u8;
                    [
                        ((x * 11) % (i32::from(alpha) + 1)) as u8,
                        ((y * 19) % (i32::from(alpha) + 1)) as u8,
                        ((x * 5 + y * 7) % (i32::from(alpha) + 1)) as u8,
                        alpha,
                    ]
                })
            })
            .collect();
        for source_format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            let source = Texture::new(
                &renderer.owner,
                17,
                13,
                source_format,
                TextureFilter::Nearest,
                false,
            )
            .unwrap();
            let bytes: Vec<_> = colors
                .iter()
                .flat_map(|color| {
                    let mut color = *color;
                    if source_format == wgt::TextureFormat::Bgra8Unorm {
                        color.swap(0, 2);
                    }
                    color
                })
                .collect();
            source
                .upload_recorded(
                    &renderer.owner,
                    &renderer.submissions,
                    DeviceIntRect::from_size(size),
                    &bytes,
                    None,
                    0,
                    None,
                )
                .unwrap();
            for format in [
                wgt::TextureFormat::Rgba8Unorm,
                wgt::TextureFormat::Bgra8Unorm,
                wgt::TextureFormat::Rgba8UnormSrgb,
                wgt::TextureFormat::Bgra8UnormSrgb,
            ] {
                let extent = wgt::Extent3d {
                    width: 19,
                    height: 15,
                    depth_or_array_layers: 1,
                };
                let usage = wgt::TextureUses::COLOR_TARGET
                    | wgt::TextureUses::RESOURCE
                    | wgt::TextureUses::COPY_SRC
                    | wgt::TextureUses::COPY_DST;
                let descriptor = texture_descriptor(extent, format, usage);
                let raw =
                    unsafe { renderer.owner.open.device.create_texture(&descriptor) }.unwrap();
                let target = Texture::from_raw(
                    &renderer.owner,
                    raw,
                    &descriptor,
                    TextureFilter::Nearest,
                    true,
                    wgt::TextureUses::UNINITIALIZED,
                )
                .unwrap();
                let pipeline = renderer.presentation_pipeline(format).unwrap();
                for (source_rect, target_rect) in [
                    ([0, 0, 17, 13], [0, 0, 13, 9]),
                    ([3, 2, 7, 5], [2, 3, 13, 9]),
                    ([2, 1, 13, 9], [3, 2, 7, 5]),
                    ([1, 1, 11, 7], [4, 5, 11, 7]),
                ] {
                    let sentinel = [23u8, 41, 67, 89];
                    target
                        .upload_recorded(
                            &renderer.owner,
                            &renderer.submissions,
                            DeviceIntRect::from_size(DeviceIntSize::new(19, 15)),
                            &sentinel.repeat(19 * 15),
                            None,
                            0,
                            None,
                        )
                        .unwrap();
                    let region =
                        PresentationRegion::new(source_rect, target_rect, [17, 13], [19, 15])
                            .unwrap();
                    renderer
                        .submissions
                        .recording()
                        .unwrap()
                        .keep(target.clone());
                    renderer
                        .record_present_draw(
                            pipeline.clone(),
                            &source,
                            &target.raw,
                            format,
                            [19, 15],
                            region,
                            target.current_usage(),
                            target.current_usage(),
                        )
                        .unwrap();
                    let pixels = super::super::shader_tests::pixels(&renderer, &target);
                    for y in 0..15 {
                        for x in 0..19 {
                            let inside = x >= target_rect[0]
                                && x < target_rect[0] + target_rect[2]
                                && y >= target_rect[1]
                                && y < target_rect[1] + target_rect[3];
                            let mut expected = sentinel;
                            if inside {
                                let sx = source_rect[0]
                                    + (((x - target_rect[0]) as f64 + 0.5) * source_rect[2] as f64
                                        / target_rect[2] as f64)
                                        .floor() as u32;
                                let sy = source_rect[1]
                                    + (((y - target_rect[1]) as f64 + 0.5) * source_rect[3] as f64
                                        / target_rect[3] as f64)
                                        .floor() as u32;
                                expected = colors[(sy * 17 + sx) as usize];
                                if matches!(
                                    format,
                                    wgt::TextureFormat::Bgra8Unorm
                                        | wgt::TextureFormat::Bgra8UnormSrgb
                                ) {
                                    expected.swap(0, 2);
                                }
                            }
                            for channel in 0..4 {
                                let actual = pixels[((y * 19 + x) * 4) as usize + channel];
                                let bound = u8::from(inside && format.is_srgb() && channel != 3);
                                assert!(
                                    actual.abs_diff(expected[channel]) <= bound,
                                    "{:?} -> {:?} {:?} {:?} ({}, {}) channel {}: {} != {}",
                                    source_format,
                                    format,
                                    source_rect,
                                    target_rect,
                                    x,
                                    y,
                                    channel,
                                    actual,
                                    expected[channel]
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
