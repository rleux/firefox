/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::*;
use api::*;
use crate::device::hal::{ExternalImageProvider, Options};
use crate::render_api::Transaction;
use crate::WebRenderOptions;
use std::cell::{Cell, RefCell};

thread_local! {
    static COMPLETION_GATE: Cell<bool> = const { Cell::new(false) };
}

fn gated_device() -> ExternalImageDevice {
    let mut device = crate::hal::create_vulkan_device(&Options {
        validation: true,
        ..Default::default()
    })
    .unwrap();
    device.completion_probe = Some(|_, _| {
        Ok(Box::new(|wait| {
            Ok(wait || COMPLETION_GATE.with(|gate| gate.get()))
        }))
    });
    ExternalImageDevice::new(&Rc::new(device))
}

fn set_completion_gate(open: bool) {
    COMPLETION_GATE.with(|gate| gate.set(open));
}

fn wait_for_hardware(device: &ExternalImageDevice) {
    let producer = device.dmabuf_producer().unwrap();
    unsafe { producer.owner.open.queue.wait_for_idle() }.unwrap();
}

struct Notice;
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self)
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    fn shut_down(&self) {}
}

struct Provider {
    image: WeakForeignRgbImage,
    uv: Rc<Cell<TexelRect>>,
}
impl ExternalImageProvider for Provider {
    fn acquire(&mut self, id: ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease> {
        assert_eq!((id.0, channel), (77, 0));
        self.image
            .upgrade()
            .ok_or("Publication released too early")?
            .lease(self.uv.get())
    }
}

#[test]
#[ignore = "Requires GL producer FDs from test_foreign_webgl.py and Intel Vulkan validation"]
fn gl_dmabuf_direct_sampling_and_release() {
    let asynchronous = std::env::var("WR_WEBGL_FORCE_SYNC").as_deref() != Ok("1");
    let number = |name: &str| std::env::var(name).unwrap().parse::<u64>().unwrap();
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FD") as i32) };
    let fence = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FENCE") as i32) };
    let ready = SyncFile::from_fd(fence.try_clone_to_owned().unwrap());
    let generation = number("WR_FOREIGN_RGB_GENERATION");
    let (width, height) = (17, 9);
    let layout = ForeignRgbLayout::new(
        [width, height],
        number("WR_FOREIGN_RGB_FOURCC") as u32,
        0,
        number("WR_FOREIGN_RGB_PITCH"),
        0,
    )
    .unwrap();
    let (mut renderer, sender) = crate::hal::create_vulkan_renderer(
        &Options {
            validation: true,
            ..Options::default()
        },
        WebRenderOptions::default(),
        Box::new(Notice),
    )
    .unwrap();
    let device = renderer.external_image_device();
    assert!(device
        .foreign_rgb_formats()
        .unwrap()
        .contains(&layout.format()));
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_foreign_rgb_dmabuf(fd, layout, &ready, generation, move |status| {
            result.borrow_mut().push(status)
        })
    }
    .unwrap();
    let acquire = device.submitted();
    assert!(acquire > 0);
    let uv = Rc::new(Cell::new(TexelRect::new(
        0.0,
        0.0,
        width as f32,
        height as f32,
    )));
    let keep = image.lease(uv.get()).unwrap();
    let unused = image.lease(uv.get()).unwrap();
    let weak = image.downgrade();
    let native = image
        .0
        .image
        .texture(&device.dmabuf_producer().unwrap().owner)
        .unwrap();
    assert!(Rc::ptr_eq(
        &native,
        &image.0.release.access.as_ref().unwrap().texture
    ));
    drop(native);
    renderer
        .set_external_image_provider(Box::new(Provider {
            image: image.downgrade(),
            uv: uv.clone(),
        }))
        .unwrap();
    drop(image);
    drop(unused);
    assert!(released.borrow().is_empty());
    assert!(weak.upgrade().is_some());
    let mut api = sender.create_api();
    let document = api.add_document(DeviceIntSize::new((width * 2 + 2) as i32, height as i32));
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    for flipped in [false, true] {
        if flipped {
            uv.set(TexelRect::new(width as f32, height as f32, 0.0, 0.0));
        }
        let mut transaction = Transaction::new();
        let descriptor = ImageDescriptor::new(
            width as i32,
            height as i32,
            layout.format().image_format(),
            ImageDescriptorFlags::IS_OPAQUE,
        );
        let external = ImageData::External(ExternalImageData {
            id: ExternalImageId(77),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        });
        if !flipped {
            transaction.add_image(key, descriptor, external, None);
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(60.0);
            let info = CommonItemProperties {
                clip_rect: LayoutRect::from_size(LayoutSize::new(
                    (width * 2 + 2) as f32,
                    height as f32,
                )),
                clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(pipeline),
                flags: PrimitiveFlags::default(),
            };
            builder.push_stacking_context(
                info.spatial_id,
                info.flags,
                None,
                TransformStyle::Flat,
                MixBlendMode::Normal,
                &[],
                &[],
                RasterSpace::Screen,
                StackingContextFlags::empty(),
                None,
            );
            for x in [0, width + 2] {
                builder.push_image(
                    &info,
                    LayoutRect::from_origin_and_size(
                        LayoutPoint::new(x as f32, 0.0),
                        LayoutSize::new(width as f32, height as f32),
                    ),
                    ImageRendering::Pixelated,
                    AlphaType::PremultipliedAlpha,
                    key,
                    ColorF::WHITE,
                );
            }
            builder.pop_stacking_context();
            transaction.set_root_pipeline(pipeline);
            transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
        } else {
            transaction.update_image(key, descriptor, external, &DirtyRect::All);
        }
        transaction.generate_frame(generation, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        renderer.prepare_frame(document).unwrap();
        renderer.render().unwrap();
        let bottom = renderer
            .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
                (width * 2 + 2) as i32,
                height as i32,
            )))
            .unwrap();
        let pixels: Vec<u8> = bottom
            .chunks_exact((width * 2 + 2) as usize * 4)
            .rev()
            .flatten()
            .copied()
            .collect();
        for y in 0..height {
            for x in 0..width {
                let (sx, sy) = if flipped {
                    (width - 1 - x, height - 1 - y)
                } else {
                    (x, y)
                };
                let expected = if (7..10).contains(&sx) && (2..5).contains(&sy) {
                    [0, 255, 0, 255]
                } else if sx < 5 && sy < 4 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, if generation == 1 { 255 } else { 0 }, 255]
                };
                for origin in [0, width + 2] {
                    let offset = ((y * (width * 2 + 2) + origin + x) * 4) as usize;
                    assert_eq!(
                        &pixels[offset..offset + 4],
                        &expected,
                        "pixel {x},{y}, flipped={flipped}"
                    );
                }
            }
        }
        renderer.poll().unwrap();
        assert!(
            released.borrow().is_empty(),
            "Unused retained lease must keep producer locked"
        );
    }
    drop(keep);
    let release = device.submitted();
    assert!(release > acquire);
    assert!(weak.upgrade().is_none());
    if asynchronous {
        assert!(released.borrow().is_empty());
        let mut completed = false;
        for _ in 0..100_000 {
            device.poll().unwrap();
            if !released.borrow().is_empty() {
                completed = true;
                break;
            }
            std::hint::spin_loop();
        }
        assert!(completed);
    }
    assert_eq!(*released.borrow(), [ExternalImageRelease::Complete]);
    api.shut_down(true);
}

#[test]
#[ignore = "Requires GL producer FDs from test_foreign_webgl.py and Intel Vulkan validation"]
fn gl_dmabuf_acquire_and_release_complete_asynchronously() {
    assert_ne!(std::env::var("WR_WEBGL_FORCE_SYNC").as_deref(), Ok("1"));
    set_completion_gate(false);
    let number = |name: &str| std::env::var(name).unwrap().parse::<u64>().unwrap();
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FD") as i32) };
    let fence = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FENCE") as i32) };
    let ready = SyncFile::from_fd(fence.try_clone_to_owned().unwrap());
    let layout = ForeignRgbLayout::new(
        [17, 9],
        number("WR_FOREIGN_RGB_FOURCC") as u32,
        0,
        number("WR_FOREIGN_RGB_PITCH"),
        0,
    )
    .unwrap();
    let device = gated_device();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_foreign_rgb_dmabuf(
            fd,
            layout,
            &ready,
            number("WR_FOREIGN_RGB_GENERATION"),
            move |status| result.borrow_mut().push(status),
        )
    }
    .unwrap();
    let acquire = device.submitted();
    assert!(acquire > 0);
    wait_for_hardware(&device);
    assert!(!device.poll_complete(acquire).unwrap());
    drop(image);
    let release = device.submitted();
    assert!(release > acquire);
    wait_for_hardware(&device);
    assert!(!device.poll_complete(release).unwrap());
    assert!(released.borrow().is_empty());
    set_completion_gate(true);
    assert!(device.poll_complete(release).unwrap());
    assert_eq!(*released.borrow(), [ExternalImageRelease::Unused]);
    assert!(device.poll_complete(release).unwrap());
    assert_eq!(released.borrow().len(), 1);
    set_completion_gate(false);
}

#[test]
#[ignore = "Requires GL producer FDs from test_foreign_webgl.py and Intel Vulkan validation"]
fn gl_dmabuf_in_flight_ownership_is_bounded() {
    assert_ne!(std::env::var("WR_WEBGL_FORCE_SYNC").as_deref(), Ok("1"));
    set_completion_gate(false);
    let number = |name: &str| std::env::var(name).unwrap().parse::<u64>().unwrap();
    let device = gated_device();
    let descriptor = ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
    let owned = (0..4)
        .map(|value| device.create_image(descriptor, &[value; 64]).unwrap())
        .collect::<Vec<_>>();
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FD") as i32) };
    let fence = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FENCE") as i32) };
    let ready = SyncFile::from_fd(fence.try_clone_to_owned().unwrap());
    let layout = ForeignRgbLayout::new(
        [17, 9],
        number("WR_FOREIGN_RGB_FOURCC") as u32,
        0,
        number("WR_FOREIGN_RGB_PITCH"),
        0,
    )
    .unwrap();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_foreign_rgb_dmabuf(
            fd,
            layout,
            &ready,
            number("WR_FOREIGN_RGB_GENERATION"),
            move |status| result.borrow_mut().push(status),
        )
    }
    .unwrap();
    let producer = device.dmabuf_producer().unwrap();
    for (index, owned) in owned.iter().enumerate() {
        let texture = owned.texture(&producer.owner).unwrap();
        texture
            .upload_recorded(
                &producer.owner,
                &producer.submissions,
                DeviceIntRect::from_size(DeviceIntSize::new(4, 4)),
                &[index as u8; 64],
                None,
                0,
                None,
            )
            .unwrap();
        producer.submissions.submit().unwrap();
        let mut stats = MemoryStats::default();
        producer.submissions.memory(&mut stats);
        assert!(stats.in_flight <= 3);
        assert!(released.borrow().is_empty());
    }
    drop(image);
    assert!(released.borrow().is_empty());
    set_completion_gate(true);
    device.finish().unwrap();
    assert_eq!(*released.borrow(), [ExternalImageRelease::Unused]);
    let mut stats = MemoryStats::default();
    producer.submissions.memory(&mut stats);
    assert_eq!(stats.in_flight, 0);
    set_completion_gate(false);
}

#[test]
#[ignore = "Requires GL producer FDs from test_foreign_webgl.py and Intel Vulkan validation"]
fn gl_dmabuf_release_failure_abandons_once() {
    assert_ne!(std::env::var("WR_WEBGL_FORCE_SYNC").as_deref(), Ok("1"));
    let number = |name: &str| std::env::var(name).unwrap().parse::<u64>().unwrap();
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FD") as i32) };
    let fence = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FENCE") as i32) };
    let ready = SyncFile::from_fd(fence.try_clone_to_owned().unwrap());
    let layout = ForeignRgbLayout::new(
        [17, 9],
        number("WR_FOREIGN_RGB_FOURCC") as u32,
        0,
        number("WR_FOREIGN_RGB_PITCH"),
        0,
    )
    .unwrap();
    let device = crate::hal::create_vulkan_image_device(&Options {
        validation: true,
        ..Default::default()
    })
    .unwrap();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_foreign_rgb_dmabuf(
            fd,
            layout,
            &ready,
            number("WR_FOREIGN_RGB_GENERATION"),
            move |status| result.borrow_mut().push(status),
        )
    }
    .unwrap();
    device
        .dmabuf_producer()
        .unwrap()
        .owner
        .fault
        .set(Some(FailurePoint::Submit));
    drop(image);
    assert!(released.borrow().is_empty());
    assert!(device.poll().is_err());
    assert_eq!(*released.borrow(), [ExternalImageRelease::Abandoned]);
    assert!(device.poll().is_err());
    assert_eq!(released.borrow().len(), 1);
}

#[test]
#[ignore = "Requires GL producer FDs from test_foreign_webgl.py and Intel Vulkan validation"]
fn gl_dmabuf_shutdown_drains_ownership_return() {
    assert_ne!(std::env::var("WR_WEBGL_FORCE_SYNC").as_deref(), Ok("1"));
    set_completion_gate(false);
    let number = |name: &str| std::env::var(name).unwrap().parse::<u64>().unwrap();
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FD") as i32) };
    let fence = unsafe { BorrowedFd::borrow_raw(number("WR_FOREIGN_RGB_FENCE") as i32) };
    let ready = SyncFile::from_fd(fence.try_clone_to_owned().unwrap());
    let layout = ForeignRgbLayout::new(
        [17, 9],
        number("WR_FOREIGN_RGB_FOURCC") as u32,
        0,
        number("WR_FOREIGN_RGB_PITCH"),
        0,
    )
    .unwrap();
    let device = gated_device();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_foreign_rgb_dmabuf(
            fd,
            layout,
            &ready,
            number("WR_FOREIGN_RGB_GENERATION"),
            move |status| result.borrow_mut().push(status),
        )
    }
    .unwrap();
    drop(image);
    assert!(released.borrow().is_empty());
    drop(device);
    assert_eq!(*released.borrow(), [ExternalImageRelease::Unused]);
}
