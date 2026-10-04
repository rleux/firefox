/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::vulkan::{Buffer, Texture, TextureFilter, Samplers};
use crate::device::vulkan::bindings::DrawBindings;
use crate::device::vulkan::draw::{Draw, DrawPass};
use crate::device::vulkan::pipeline::DrawPipeline;
use crate::device::vulkan::shader::select_draw_shader;
use crate::device::RenderState;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};

#[path = "external_access_tests.rs"]
mod registry;

fn publish(
    source: &Rc<DmaBufImage>,
    ready: &Rc<SharedTimeline>,
    value: u64,
    released: Option<(&Rc<SharedTimeline>, u64)>,
    color: [f32; 4],
) -> Submission {
    let owner = &source.owner;
    let mut send = Submission::new(owner).unwrap();
    {
        let mut recording = send.recording().unwrap();
        if let Some((timeline, point)) = released {
            recording.wait_timeline(timeline, point).unwrap();
            barrier(
                &mut recording,
                source,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_EXTERNAL,
                owner.open.device.queue_family_index(),
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
            );
        } else {
            barrier(
                &mut recording,
                source,
                vk::ImageLayout::UNDEFINED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
            );
        }
        unsafe {
            owner.open.device.raw_device().cmd_clear_color_image(
                recording.encoder().raw_handle(),
                source.image,
                vk::ImageLayout::GENERAL,
                &vk::ClearColorValue { float32: color },
                &[vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1)],
            );
        }
        barrier(
            &mut recording,
            source,
            vk::ImageLayout::GENERAL,
            owner.open.device.queue_family_index(),
            vk::QUEUE_FAMILY_EXTERNAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
        );
        recording.signal_timeline(ready, value).unwrap();
    }
    send.submit().unwrap();
    send
}

fn floats(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

fn sampled_draw(owner: &Rc<Device>, texture: Rc<Texture>) -> (Rc<Buffer>, Draw) {
    let shader = select_draw_shader("cs_scale", &["TEXTURE_2D"], false).unwrap();
    let pipeline = DrawPipeline::new(
        owner,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .unwrap();
    let projection = Buffer::new(
        owner,
        &floats(&[
            1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
        ]),
        wgt::BufferUses::UNIFORM,
    )
    .unwrap();
    let bindings = DrawBindings::new(
        &pipeline,
        Some(projection),
        vec![(texture.clone(), texture.filter())],
        Vec::new(),
        Some(Rc::new(Samplers::new(owner).unwrap())),
    )
    .unwrap();
    let quad = Buffer::new(
        owner,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let instances = Buffer::new(
        owner,
        &floats(&[-1., -1., 1., 1., 0., 0., 2., 2., 0.]),
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    (
        quad,
        Draw {
            bindings,
            instances,
            instance_offset: 0,
            instance_count: 1,
            scissor: DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
        },
    )
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF sampling, shared timelines and validation"]
fn dma_buf_access_samples_updates_and_releases_through_the_draw_path() {
    {
        let producer = device();
        let consumer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(&consumer).unwrap();
        let release_import =
            SharedTimeline::import(&producer, &released.export().unwrap()).unwrap();
        let mut value = 0;
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            let usages = wgt::TextureUses::RESOURCE
                | wgt::TextureUses::COPY_SRC
                | wgt::TextureUses::COPY_DST;
            let formats = producer.dma_buf_formats(format, usages).unwrap();
            assert!(formats
                .iter()
                .any(|entry| entry.modifier() == 0 && entry.exportable()));
            assert!(formats
                .iter()
                .any(|entry| entry.modifier() != 0 && entry.exportable()));
            for caps in formats.iter().filter(|entry| entry.exportable()) {
                let (source, fd) = export_image(&producer, format, caps.modifier(), [2, 2]);
                let image =
                    unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
                let texture = Texture::from_dma_buf(&image, TextureFilter::Linear).unwrap();
                assert!(!texture.initialized());
                assert_eq!(texture.size().width, 2);
                let (quad, draw) = sampled_draw(&consumer, texture.clone());
                let target = Texture::new(
                    &consumer,
                    2,
                    2,
                    wgt::TextureFormat::Rgba8Unorm,
                    TextureFilter::Nearest,
                    true,
                )
                .unwrap();
                for channel in 0..3 {
                    value += 1;
                    let mut color = [0., 0., 0., 1.];
                    color[channel] = 1.;
                    let mut send = publish(
                        &source,
                        &ready,
                        value,
                        if channel == 0 {
                            None
                        } else {
                            Some((&release_import, value - 1))
                        },
                        color,
                    );
                    let mut receive = Submission::new(&consumer).unwrap();
                    {
                        let mut recording = receive.recording().unwrap();
                        unsafe { image.acquire(&mut recording, &ready_import, value) }.unwrap();
                        assert!(texture.sample_initialized());
                        DrawPass {
                            target: &target,
                            origin: DeviceIntPoint::zero(),
                            viewport: None,
                            depth: None,
                            clear_color: None,
                            clear_depth: None,
                            depth_range: 0.0..1.0,
                        }
                        .record(&mut recording, &quad, std::slice::from_ref(&draw))
                        .unwrap();
                        image.release(&mut recording, &released, value).unwrap();
                        assert!(!texture.sample_initialized());
                        assert!(texture
                            .transition(&mut recording, wgt::TextureUses::RESOURCE)
                            .is_err());
                    }
                    receive.submit().unwrap();
                    assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
                    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
                    let result = target
                        .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 2)))
                        .unwrap()
                        .wait()
                        .unwrap();
                    let mut pixel = [0, 0, 0, 255];
                    pixel[channel] = 255;
                    assert_eq!(result, pixel.repeat(4));
                }
            }
        }
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images, shared timelines and validation"]
fn dma_buf_access_rolls_back_and_shares_state_between_views() {
    {
        let producer = device();
        let consumer = device();
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(&consumer).unwrap();
        let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
        let mut send = publish(&source, &ready, 1, None, [1., 0., 0., 1.]);
        let image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
        assert!(Texture::from_dma_buf(&image, TextureFilter::Trilinear).is_err());
        let first = Texture::from_dma_buf(&image, TextureFilter::Nearest).unwrap();
        let second = Texture::from_dma_buf(&image, TextureFilter::Linear).unwrap();
        let mut acquire = Submission::new(&consumer).unwrap();
        {
            let mut recording = acquire.recording().unwrap();
            assert!(image.release(&mut recording, &released, 1).is_err());
            assert!(first
                .transition(&mut recording, wgt::TextureUses::RESOURCE)
                .is_err());
            unsafe { image.acquire(&mut recording, &ready_import, 1) }.unwrap();
            assert!(unsafe { image.acquire(&mut recording, &ready_import, 1) }.is_err());
            assert!(first.initialized() && second.initialized());
            assert!(first
                .transition(&mut recording, wgt::TextureUses::COPY_DST)
                .is_err());
            assert!(first.invalidate(&mut recording).is_err());
            assert!(first.initialize(&mut recording).is_err());
            let mut other = Submission::new(&consumer).unwrap();
            assert!(second
                .transition(&mut other.recording().unwrap(), wgt::TextureUses::RESOURCE)
                .is_err());
        }
        drop(acquire);
        assert!(!first.initialized() && !second.initialized());
        let mut acquire = Submission::new(&consumer).unwrap();
        unsafe { image.acquire(&mut acquire.recording().unwrap(), &ready_import, 1) }.unwrap();
        acquire.submit().unwrap();
        assert!(acquire.wait(Some(Duration::from_secs(5))).unwrap());
        let mut cancelled_release = Submission::new(&consumer).unwrap();
        image
            .release(&mut cancelled_release.recording().unwrap(), &released, 1)
            .unwrap();
        assert!(!second.initialized());
        drop(cancelled_release);
        assert!(first.initialized() && second.initialized());
        let mut release = Submission::new(&consumer).unwrap();
        image
            .release(&mut release.recording().unwrap(), &released, 1)
            .unwrap();
        let weak = Rc::downgrade(&image);
        drop(image);
        drop(first);
        drop(second);
        release.submit().unwrap();
        assert!(weak.upgrade().is_some());
        assert!(release.wait(Some(Duration::from_secs(5))).unwrap());
        assert!(weak.upgrade().is_none());
        assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images and validation"]
fn dma_buf_access_view_retains_the_native_allocation() {
    {
        let producer = device();
        let consumer = device();
        let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
        let image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
        let weak = Rc::downgrade(&image);
        let weak_device = Rc::downgrade(&consumer);
        let first = Texture::from_dma_buf(&image, TextureFilter::Nearest).unwrap();
        let second = Texture::from_dma_buf(&image, TextureFilter::Linear).unwrap();
        drop(image);
        drop(consumer);
        drop(source);
        drop(producer);
        drop(fd);
        assert!(weak.upgrade().is_some());
        drop(first);
        assert!(weak.upgrade().is_some());
        drop(second);
        assert!(weak.upgrade().is_none());
        assert!(weak_device.upgrade().is_none());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
