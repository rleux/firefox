/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::wgpu::{
    ExternalReleaseStatus, ExternalTextureRegistry, ForeignRgbLayout, NativeImage,
    PendingExternalRelease,
};
use std::{
    cell::{Cell, RefCell},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    rc::Rc,
    time::{Duration, Instant},
};

struct ForeignHandler {
    registry: Rc<ExternalTextureRegistry>,
    image: Rc<NativeImage>,
    ready: Rc<RefCell<Option<OwnedFd>>>,
    handles: [ExternalTextureHandle; 2],
    locks: Rc<Cell<usize>>,
    acquired: bool,
    receipt: Rc<RefCell<Option<PendingExternalRelease>>>,
    pending_once: Rc<Cell<bool>>,
}

impl ExternalImageHandler for ForeignHandler {
    fn lock(&mut self, _: ExternalImageId, channel: u8, _: bool) -> ExternalImage<'_> {
        assert!(channel < 2);
        self.locks.set(self.locks.get() + 1);
        let ready = self.ready.borrow();
        let acquired = unsafe {
            self.registry
                .acquire_foreign(&self.image, ready.as_ref().unwrap().as_fd())
        }
        .unwrap();
        self.acquired |= acquired;
        ExternalImage {
            uv: TexelRect::new(0., 0., 17., 9.),
            source: if acquired && !self.pending_once.replace(false) {
                ExternalImageSource::NativeTexture(self.handles[channel as usize])
            } else {
                ExternalImageSource::Pending
            },
        }
    }

    fn unlock(&mut self, _: ExternalImageId, _: u8) {
        assert!(self.locks.get() > 0);
        self.locks.set(self.locks.get() - 1);
        if self.locks.get() == 0 && self.acquired {
            self.acquired = false;
            *self.receipt.borrow_mut() = Some(self.registry.release(&self.image).unwrap());
        }
    }
}

fn receive_foreign(socket: &OwnedFd) -> (Vec<u64>, Vec<OwnedFd>) {
    let mut bytes = [0u8; 256];
    let mut control = [0u64; 16];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    let size = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    assert!(size > 0, "recvmsg: {}", std::io::Error::last_os_error());
    assert_eq!(message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC), 0);
    let mut fds = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            assert_eq!((*header).cmsg_level, libc::SOL_SOCKET);
            assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
            let length = (*header).cmsg_len - libc::CMSG_LEN(0) as usize;
            assert_eq!(length % std::mem::size_of::<i32>(), 0);
            let data = libc::CMSG_DATA(header).cast::<i32>();
            for index in 0..length / std::mem::size_of::<i32>() {
                fds.push(OwnedFd::from_raw_fd(*data.add(index)));
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    assert_eq!(fds.len(), 3);
    let values = std::str::from_utf8(&bytes[..size as usize])
        .unwrap()
        .split_whitespace()
        .map(|value| value.parse().unwrap())
        .collect::<Vec<u64>>();
    assert_eq!(values.len(), 8);
    (values, fds)
}

fn foreign_stat(fd: &OwnedFd) -> libc::stat {
    let mut stat = std::mem::MaybeUninit::uninit();
    assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) }, 0);
    unsafe { stat.assume_init() }
}

fn reply_foreign(socket: &OwnedFd, reply: &[u8]) {
    assert_eq!(
        unsafe {
            libc::send(
                socket.as_raw_fd(),
                reply.as_ptr().cast(),
                reply.len(),
                libc::MSG_NOSIGNAL,
            )
        },
        reply.len() as isize
    );
}

fn foreign_pixel(x: usize, y: usize, cycle: u32) -> [u8; 4] {
    let cycle = cycle as usize;
    [
        ((x * 11 + cycle * 43) % 112 + 8) as u8,
        ((y * 23 + cycle * 17) % 112 + 8) as u8,
        ((x * 7 + y * 13 + cycle * 29) % 112 + 8) as u8,
        128,
    ]
}

#[test]
#[ignore = "Requires an inherited real GL producer socket, DMA-BUF and modern sync validation"]
fn vulkan_renderer_foreign_gl_reuses_only_after_complete() {
    let socket = std::env::var("WR_FOREIGN_SOCKET_FD").expect("Run producer.py --renderer-foreign");
    let socket = unsafe { OwnedFd::from_raw_fd(socket.parse().unwrap()) };
    let failure = std::env::var("WR_FOREIGN_FAILURE").unwrap();
    assert!(matches!(failure.as_str(), "validation" | "loss"));
    let (mut renderer, mut api, rx) = renderer();
    let registry = renderer.wgpu_external_textures().unwrap();
    let ready = Rc::new(RefCell::new(None));
    let receipt: Rc<RefCell<Option<PendingExternalRelease>>> = Rc::new(RefCell::new(None));
    let pending_once = Rc::new(Cell::new(false));
    let locks = Rc::new(Cell::new(0));
    let size = DeviceIntSize::new(34, 9);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let keys = [api.generate_image_key(), api.generate_image_key()];
    let mut image: Option<Rc<NativeImage>> = None;
    let mut handles = None;
    let mut allocation_identity = None;
    let mut native_identities = None;

    for cycle in 0..=8u32 {
        let (values, mut fds) = receive_foreign(&socket);
        assert_eq!(&values[..2], &[17, 9]);
        assert_eq!(values[3], 0);
        assert_eq!(values[5], 0);
        assert_eq!(values[6], u64::from(cycle));
        assert_eq!(values[7], 0);
        let node = fds.pop().unwrap();
        let fence = fds.pop().unwrap();
        let memory = fds.pop().unwrap();
        let stat = foreign_stat(&memory);
        let identity = (stat.st_dev, stat.st_ino, stat.st_size, values[2], values[4]);
        if let Some(previous) = allocation_identity {
            assert_eq!(
                previous, identity,
                "producer changed the allocation or layout"
            );
        } else {
            allocation_identity = Some(identity);
        }
        if cycle != 0 {
            assert_eq!(
                receipt.borrow().as_ref().unwrap().status(),
                ExternalReleaseStatus::Complete
            );
        }
        *ready.borrow_mut() = Some(fence);
        if image.is_none() {
            let layout = ForeignRgbLayout::new([17, 9], values[2] as u32, 0, values[4], 0).unwrap();
            let allocation = unsafe {
                registry
                    .device()
                    .import_foreign_rgb(memory.as_fd(), node.as_fd(), layout)
            }
            .unwrap();
            let imported = registry.import_foreign(allocation).unwrap();
            let registered = [
                registry.register(&imported, false).unwrap(),
                registry.register(&imported, true).unwrap(),
            ];
            renderer.set_external_image_handler(Box::new(ForeignHandler {
                registry: registry.clone(),
                image: imported.clone(),
                ready: ready.clone(),
                handles: registered,
                locks: locks.clone(),
                acquired: false,
                receipt: receipt.clone(),
                pending_once: pending_once.clone(),
            }));
            image = Some(imported);
            handles = Some(registered);
        }
        let image = image.as_ref().unwrap();
        pending_once.set(cycle == 1);
        let mut transaction = Transaction::new();
        if cycle == 0 {
            for channel in 0..2 {
                transaction.add_image(
                    keys[channel],
                    ImageDescriptor::new(17, 9, ImageFormat::RGBA8, ImageDescriptorFlags::empty()),
                    ImageData::External(ExternalImageData {
                        id: ExternalImageId(177),
                        channel_index: channel as u8,
                        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                        normalized_uvs: false,
                    }),
                    None,
                );
            }
        }
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let full = LayoutRect::from_size(LayoutSize::new(34., 9.));
        let common = CommonItemProperties::new(full, SpaceAndClipInfo::root_scroll(pipeline));
        builder.push_rect(&common, full, ColorF::new(1., 0., 0., 1.));
        for channel in 0..2 {
            builder.push_image(
                &common,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(channel as f32 * 17., 0.),
                    LayoutSize::new(17., 9.),
                ),
                ImageRendering::Pixelated,
                AlphaType::PremultipliedAlpha,
                keys[channel],
                ColorF::WHITE,
            );
        }
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(cycle), api.get_namespace_id(), builder.end());
        transaction.generate_frame(u64::from(cycle) + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(Duration::from_secs(15)).unwrap();
        renderer.update();

        if cycle == 8 {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !unsafe {
                registry.acquire_foreign(image, ready.borrow().as_ref().unwrap().as_fd())
            }
            .unwrap()
            {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(!renderer.render(size, 0).unwrap().external_images_pending);
            if failure == "validation" {
                renderer.device.begin_frame();
                renderer.device.wgpu_test_fail_submit();
                renderer.device.finish_resource_work();
                assert!(renderer.device.failure().is_some());
            } else {
                renderer.device.wgpu_test_lose_device();
            }
            assert_eq!(locks.get(), 0);
            assert_eq!(
                receipt.borrow().as_ref().unwrap().status(),
                ExternalReleaseStatus::Abandoned
            );
            reply_foreign(&socket, b"ABANDONED");
            eprintln!("WR_GL_ABANDONED PASS failure={failure} no producer reuse");
            break;
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pending = 0;
        loop {
            let result = renderer.render(size, 0).unwrap();
            assert_eq!(locks.get(), 0);
            if !result.external_images_pending {
                break;
            }
            pending += 1;
            assert!(
                Instant::now() < deadline,
                "renderer remained pending cycle={}",
                cycle
            );
            assert!(!renderer.active_documents[&document].frame.has_been_rendered);
            assert_eq!(result.present_result, Some(crate::PresentResult::Retry));
            renderer.gpu_submission_status().unwrap();
            std::thread::yield_now();
        }
        if cycle < 2 {
            assert!(pending > 0);
        }
        let identities = image.test_native_identities();
        assert!(identities.1.is_some());
        if let Some(previous) = native_identities {
            assert_eq!(
                previous, identities,
                "allocation/core texture recreated during reuse"
            );
        } else {
            native_identities = Some(identities);
        }
        let pixels = capture(&mut renderer, size);
        assert_eq!(pixels.len(), 34 * 9 * 4);
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            let x = index % 34;
            let mut expected = foreign_pixel(x % 17, index / 34, cycle);
            if x < 17 {
                expected[0] += 127;
            }
            expected[3] = 255;
            assert!(
                pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                "cycle={} pixel={} actual={:?} expected={:?}",
                cycle,
                index,
                pixel,
                expected
            );
        }
        eprintln!("WR_GL_PIXELS PASS cycle={cycle} pixels=306 normal=153 opaque=153 native={identities:?}");
        #[cfg(feature = "capture")]
        {
            assert_eq!(image.descriptor().usage, crate::wgpu::TextureUses::RESOURCE);
            while !unsafe {
                registry.acquire_foreign(image, ready.borrow().as_ref().unwrap().as_fd())
            }
            .unwrap()
            {
                assert!(
                    Instant::now() < deadline,
                    "capture acquisition stayed pending"
                );
                registry.flush_idle().unwrap();
                renderer.gpu_submission_status().unwrap();
                std::thread::yield_now();
            }
            let _pending_core_write =
                crate::wgpu::native_test::queue_capture_write(registry.device());
            for format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
                let descriptor = ImageDescriptor::new(17, 9, format, ImageDescriptorFlags::empty());
                for (channel, handle) in handles.unwrap().iter().copied().enumerate() {
                    let captured = renderer.device.read_external_texture(
                        handle,
                        ImageBufferKind::Texture2D,
                        &descriptor,
                    );
                    let mut expected = Vec::with_capacity(17 * 9 * 4);
                    for y in 0..9 {
                        for x in 0..17 {
                            let mut pixel = foreign_pixel(x, y, cycle);
                            if channel == 1 {
                                pixel[3] = 255;
                            }
                            if format == ImageFormat::BGRA8 {
                                pixel.swap(0, 2);
                            }
                            expected.extend_from_slice(&pixel);
                        }
                    }
                    assert_eq!(
                        captured, expected,
                        "capture cycle={cycle} format={format:?} channel={channel}"
                    );
                }
            }
            assert_eq!(image.descriptor().usage, crate::wgpu::TextureUses::RESOURCE);
            assert_eq!(image.test_native_identities(), identities);
            *receipt.borrow_mut() = Some(registry.release(image).unwrap());
            registry.flush_idle().unwrap();
            eprintln!("WR_GL_CAPTURE PASS cycle={cycle} pixels=612 source=RESOURCE formats=RGBA8,BGRA8 views=normal,opaque");
        }
        let returned = receipt.borrow().as_ref().unwrap().clone();
        loop {
            match returned.status() {
                ExternalReleaseStatus::Complete => break,
                ExternalReleaseStatus::Abandoned => panic!("healthy GL ownership return abandoned"),
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "GL return remained pending cycle={}",
                cycle
            );
            registry.flush_idle().unwrap();
            renderer.gpu_submission_status().unwrap();
            std::thread::yield_now();
        }
        reply_foreign(&socket, b"RETURN_COMPLETE");
        eprintln!("WR_GL_RETURN_COMPLETE PASS cycle={cycle} receipt=Complete FOREIGN/core done");
    }
    for handle in handles.unwrap() {
        registry.unregister(handle).unwrap();
    }
    api.delete_document(document);
    renderer.deinit();
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::wgpu::poll_retired_native_images() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
