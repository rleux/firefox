/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::{resources::Texture, submission::Submission};
use super::*;
use std::rc::Rc;
type V = hal::api::Vulkan;

pub(super) struct Conversion {
    owner: Rc<Device<V>>,
    pipeline: vk::Pipeline,
    framebuffer: vk::Framebuffer,
    pass: vk::RenderPass,
    pool: vk::DescriptorPool,
    layout: vk::PipelineLayout,
    set_layout: vk::DescriptorSetLayout,
    source_view: vk::ImageView,
    target_view: vk::ImageView,
    sampler: vk::Sampler,
    conversion: vk::SamplerYcbcrConversion,
    modules: Vec<vk::ShaderModule>,
    set: vk::DescriptorSet,
    target: Rc<Texture<V>>,
    _source: Rc<dyn std::any::Any>,
}

impl Drop for Conversion {
    fn drop(&mut self) {
        let raw = self.owner.open.device.raw_device();
        unsafe {
            raw.destroy_pipeline(self.pipeline, None);
            raw.destroy_framebuffer(self.framebuffer, None);
            raw.destroy_render_pass(self.pass, None);
            raw.destroy_descriptor_pool(self.pool, None);
            raw.destroy_pipeline_layout(self.layout, None);
            raw.destroy_descriptor_set_layout(self.set_layout, None);
            raw.destroy_image_view(self.source_view, None);
            raw.destroy_image_view(self.target_view, None);
            raw.destroy_sampler(self.sampler, None);
            raw.destroy_sampler_ycbcr_conversion(self.conversion, None);
            for module in &self.modules {
                raw.destroy_shader_module(*module, None);
            }
        }
    }
}

impl Conversion {
    pub(super) unsafe fn new(
        owner: &Rc<Device<V>>,
        source: vk::Image,
        format: vk::Format,
        conversion: Option<&vk::SamplerYcbcrConversionCreateInfo<'_>>,
        lifetime: Rc<dyn std::any::Any>,
        target: Rc<Texture<V>>,
    ) -> Result<Rc<Self>> {
        let mut result = Self {
            owner: owner.clone(),
            pipeline: vk::Pipeline::null(),
            framebuffer: vk::Framebuffer::null(),
            pass: vk::RenderPass::null(),
            pool: vk::DescriptorPool::null(),
            layout: vk::PipelineLayout::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            source_view: vk::ImageView::null(),
            target_view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
            conversion: vk::SamplerYcbcrConversion::null(),
            modules: Vec::new(),
            set: vk::DescriptorSet::null(),
            target,
            _source: lifetime,
        };
        let raw = owner.open.device.raw_device();
        if let Some(info) = conversion {
            result.conversion = raw
                .create_sampler_ycbcr_conversion(info, None)
                .map_err(|e| format!("Creating native YCbCr conversion: {e:?}"))?;
        }
        let mut ycbcr = vk::SamplerYcbcrConversionInfo::default().conversion(result.conversion);
        let mut sampler = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        if conversion.is_some() {
            sampler = sampler.push_next(&mut ycbcr);
        }
        result.sampler = raw
            .create_sampler(&sampler, None)
            .map_err(|e| format!("Creating native sampler: {e:?}"))?;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let mut ycbcr_view =
            vk::SamplerYcbcrConversionInfo::default().conversion(result.conversion);
        let mut view = vk::ImageViewCreateInfo::default()
            .image(source)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(range);
        if conversion.is_some() {
            view = view.push_next(&mut ycbcr_view);
        }
        result.source_view = raw
            .create_image_view(&view, None)
            .map_err(|e| format!("Creating native image view: {e:?}"))?;
        result.target_view = raw
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(result.target.raw.raw_handle())
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(range),
                None,
            )
            .map_err(|e| format!("Creating converted image view: {e:?}"))?;
        let samplers = [result.sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        result.set_layout = raw
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
            .map_err(|e| format!("Creating native conversion bindings: {e:?}"))?;
        let layouts = [result.set_layout];
        let constants = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .size(4)];
        result.layout = raw
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&constants),
                None,
            )
            .map_err(|e| format!("Creating native conversion layout: {e:?}"))?;
        // External formats can consume multiple descriptors; reserve the four-component upper bound.
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(4)];
        result.pool = raw
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )
            .map_err(|e| format!("Creating native conversion pool: {e:?}"))?;
        result.set = raw
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(result.pool)
                    .set_layouts(&layouts),
            )
            .map_err(|e| format!("Allocating native conversion bindings: {e:?}"))?[0];
        let images = [vk::DescriptorImageInfo::default()
            .image_view(result.source_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        raw.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(result.set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&images)],
            &[],
        );
        let attachments = [vk::AttachmentDescription::default()
            .format(vk::Format::R8G8B8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let colors = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colors)];
        result.pass = raw
            .create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachments)
                    .subpasses(&subpasses),
                None,
            )
            .map_err(|e| format!("Creating native conversion pass: {e:?}"))?;
        let views = [result.target_view];
        result.framebuffer = raw
            .create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(result.pass)
                    .attachments(&views)
                    .width(result.target.size.width)
                    .height(result.target.size.height)
                    .layers(1),
                None,
            )
            .map_err(|e| format!("Creating native conversion framebuffer: {e:?}"))?;
        for bytes in [
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/hal-shaders/hal_convert.vert.spv"
            ))
            .as_slice(),
            include_bytes!(concat!(
                env!("OUT_DIR"),
                "/hal-shaders/hal_convert.frag.spv"
            ))
            .as_slice(),
        ] {
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
                .collect();
            result.modules.push(
                raw.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                    .map_err(|e| format!("Creating native conversion shader: {e:?}"))?,
            );
        }
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(result.modules[0])
                .name(CStr::from_bytes_with_nul(b"main\0").unwrap()),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(result.modules[1])
                .name(CStr::from_bytes_with_nul(b"main\0").unwrap()),
        ];
        let vertex = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = [vk::Viewport::default()
            .width(result.target.size.width as f32)
            .height(result.target.size.height as f32)
            .max_depth(1.0)];
        let scissors = [vk::Rect2D::default().extent(vk::Extent2D {
            width: result.target.size.width,
            height: result.target.size.height,
        })];
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewport)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .line_width(1.0);
        let samples = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blends = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
        result.pipeline = match raw.create_graphics_pipelines(
            vk::PipelineCache::null(),
            &[vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex)
                .input_assembly_state(&assembly)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&samples)
                .color_blend_state(&blend)
                .layout(result.layout)
                .render_pass(result.pass)],
            None,
        ) {
            Ok(pipelines) => pipelines[0],
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    raw.destroy_pipeline(pipeline, None);
                }
                return Err(format!("Creating native conversion pipeline: {error:?}"));
            }
        };
        Ok(Rc::new(result))
    }

    pub(super) unsafe fn record(self: &Rc<Self>, commands: &mut Submission<V>, alpha: u32) {
        commands.keep(self.clone());
        self.target
            .transition(commands, wgt::TextureUses::COLOR_TARGET);
        let raw = self.owner.open.device.raw_device();
        let command = commands.encoder().raw_handle();
        raw.cmd_begin_render_pass(
            command,
            &vk::RenderPassBeginInfo::default()
                .render_pass(self.pass)
                .framebuffer(self.framebuffer)
                .render_area(vk::Rect2D::default().extent(vk::Extent2D {
                    width: self.target.size.width,
                    height: self.target.size.height,
                })),
            vk::SubpassContents::INLINE,
        );
        raw.cmd_bind_pipeline(command, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
        raw.cmd_bind_descriptor_sets(
            command,
            vk::PipelineBindPoint::GRAPHICS,
            self.layout,
            0,
            &[self.set],
            &[],
        );
        raw.cmd_push_constants(
            command,
            self.layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &alpha.to_ne_bytes(),
        );
        raw.cmd_draw(command, 3, 1, 0, 0);
        raw.cmd_end_render_pass(command);
        self.target.initialize(commands);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::resources::Buffer;
    use super::*;

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn native_conversion_rgba_orientation_alpha_and_retirement() {
        let owner = Rc::new(
            create_vulkan_device(&Options {
                adapter_name: None,
                validation: true,
            })
            .unwrap(),
        );
        let device = ExternalImageDevice::new(&owner);
        let producer = device.0.as_any().downcast_ref::<Producer<V>>().unwrap();
        let descriptor = api::ImageDescriptor::new(
            17,
            13,
            api::ImageFormat::RGBA8,
            api::ImageDescriptorFlags::empty(),
        );
        let pixels: Vec<u8> = (0..17 * 13)
            .flat_map(|i| [(i % 17 * 7) as u8, (i / 17 * 9) as u8, 64, 128])
            .collect();
        let image = device.create_image(descriptor, &pixels).unwrap();
        let source = image.texture(&owner).unwrap();
        for alpha in 0..3 {
            let target = Texture::new(
                &owner,
                17,
                13,
                wgt::TextureFormat::Rgba8Unorm,
                crate::device::TextureFilter::Linear,
                true,
            )
            .unwrap();
            let conversion = unsafe {
                Conversion::new(
                    &owner,
                    source.raw.raw_handle(),
                    vk::Format::R8G8B8A8_UNORM,
                    None,
                    source.clone(),
                    target.clone(),
                )
            }
            .unwrap();
            let weak = Rc::downgrade(&conversion);
            {
                let mut commands = producer.submissions.recording().unwrap();
                source.transition(&mut commands, wgt::TextureUses::RESOURCE);
                unsafe {
                    conversion.record(&mut commands, alpha);
                }
            }
            drop(conversion);
            assert!(weak.upgrade().is_some());
            let layout = owner.layout(17, 13).unwrap();
            let readback = Buffer::readback(&owner, &layout).unwrap();
            {
                let mut commands = producer.submissions.recording().unwrap();
                target.transition(&mut commands, wgt::TextureUses::COPY_SRC);
                readback.transition(&mut commands, wgt::BufferUses::COPY_DST);
                unsafe {
                    copy_readback::<V>(
                        commands.encoder(),
                        &target.raw,
                        &readback.raw,
                        &layout,
                        target.size,
                        hal::FormatAspects::COLOR,
                    );
                }
                readback.transition(&mut commands, wgt::BufferUses::MAP_READ);
            }
            producer.submissions.wait().unwrap();
            assert!(weak.upgrade().is_none());
            let actual = owner.map_readback(&readback.raw, &layout).unwrap();
            for (actual, source) in actual.chunks_exact(4).zip(pixels.chunks_exact(4)) {
                assert_eq!(actual[3], if alpha == 0 { 255 } else { source[3] });
                for channel in 0..3 {
                    let expected = if alpha == 2 {
                        (u32::from(source[channel]) * 128 + 127) / 255
                    } else {
                        u32::from(source[channel])
                    };
                    assert_eq!(
                        u32::from(actual[channel]),
                        expected,
                        "alpha={alpha} channel={channel}"
                    );
                }
            }
        }
        println!("Native conversion: odd extent/orientation, opaque/premultiplied/straight alpha and GPU retirement passed");
    }
}
