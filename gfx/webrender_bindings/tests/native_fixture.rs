/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::vk;
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{mpsc, Arc},
    time::Duration,
};
use webrender::{
    api::{units::*, *},
    render_api::{RenderApi, Transaction},
    wgpu::{Device, Options},
};

#[repr(C)]
pub struct TestNativeImage {
    memory_fd: i32,
    ready_fd: i32,
    offset: u64,
    stride: u64,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}

struct Producer {
    owner: Arc<Device>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    ready: vk::Semaphore,
    pool: vk::CommandPool,
    memory_fd: OwnedFd,
    ready_fd: OwnedFd,
    offset: u64,
    stride: u64,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    last: u64,
    alpha: f32,
}
impl Producer {
    fn new() -> Self {
        let owner = Arc::new(
            Device::new(&Options {
                validation: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let hal = owner.raw_device();
        let raw = hal.raw_device();
        let mut ids = vk::PhysicalDeviceIDProperties::default();
        unsafe {
            hal.shared_instance().raw_instance().get_physical_device_properties2(
                hal.raw_physical_device(),
                &mut vk::PhysicalDeviceProperties2::default().push_next(&mut ids),
            );
        }
        let modifiers = [0];
        let mut modifier = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image = unsafe {
            raw.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width: 4,
                        height: 4,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(
                        vk::ImageUsageFlags::SAMPLED
                            | vk::ImageUsageFlags::TRANSFER_SRC
                            | vk::ImageUsageFlags::TRANSFER_DST,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut modifier)
                    .push_next(&mut external),
                None,
            )
        }
        .unwrap();
        let req = unsafe { raw.get_image_memory_requirements(image) };
        let props = unsafe {
            hal.shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(hal.raw_physical_device())
        };
        let index = (0..props.memory_type_count)
            .find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && !props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::PROTECTED)
            })
            .unwrap();
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut export =
            vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let memory = unsafe {
            raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(index)
                    .push_next(&mut dedicated)
                    .push_next(&mut export),
                None,
            )
        }
        .unwrap();
        unsafe { raw.bind_image_memory(image, memory, 0) }.unwrap();
        let layout = unsafe {
            raw.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            )
        };
        let ext = ash::khr::external_memory_fd::Device::new(hal.shared_instance().raw_instance(), raw);
        let memory_fd = unsafe {
            OwnedFd::from_raw_fd(
                ext.get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )
                .unwrap(),
            )
        };
        let mut ty = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
        let mut export =
            vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        let ready = unsafe {
            raw.create_semaphore(
                &vk::SemaphoreCreateInfo::default()
                    .push_next(&mut ty)
                    .push_next(&mut export),
                None,
            )
        }
        .unwrap();
        let ext = ash::khr::external_semaphore_fd::Device::new(hal.shared_instance().raw_instance(), raw);
        let ready_fd = unsafe {
            OwnedFd::from_raw_fd(
                ext.get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default()
                        .semaphore(ready)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
                )
                .unwrap(),
            )
        };
        let pool = unsafe {
            raw.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(hal.queue_family_index()),
                None,
            )
        }
        .unwrap();
        drop(hal);
        Self {
            owner,
            image,
            memory,
            ready,
            pool,
            memory_fd,
            ready_fd,
            offset: layout.offset,
            stride: layout.row_pitch,
            device_uuid: ids.device_uuid,
            driver_uuid: ids.driver_uuid,
            last: 0,
            alpha: 1.,
        }
    }
    fn publish(&mut self, value: u64, returned_fd: i32, returned_value: u64) {
        let hal = self.owner.raw_device();
        let raw = hal.raw_device();
        let family = hal.queue_family_index();
        let returned = if returned_fd >= 0 {
            let mut ty = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            let sem =
                unsafe { raw.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty), None) }.unwrap();
            let fd = unsafe { libc::dup(returned_fd) };
            assert!(fd >= 0);
            unsafe {
                ash::khr::external_semaphore_fd::Device::new(hal.shared_instance().raw_instance(), raw)
                    .import_semaphore_fd(
                        &vk::ImportSemaphoreFdInfoKHR::default()
                            .semaphore(sem)
                            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
                            .fd(fd),
                    )
            }
            .unwrap();
            Some(sem)
        } else {
            None
        };
        let command = unsafe {
            raw.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .unwrap()[0];
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        unsafe {
            raw.begin_command_buffer(command, &vk::CommandBufferBeginInfo::default())
                .unwrap();
            raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(self.image)
                    .subresource_range(range)
                    .old_layout(if self.last == 0 {
                        vk::ImageLayout::UNDEFINED
                    } else {
                        vk::ImageLayout::GENERAL
                    })
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(if self.last == 0 {
                        vk::QUEUE_FAMILY_IGNORED
                    } else {
                        vk::QUEUE_FAMILY_EXTERNAL
                    })
                    .dst_queue_family_index(if self.last == 0 {
                        vk::QUEUE_FAMILY_IGNORED
                    } else {
                        family
                    })
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
            );
            raw.cmd_clear_color_image(
                command,
                self.image,
                vk::ImageLayout::GENERAL,
                &vk::ClearColorValue {
                    float32: [1., 0., 0., self.alpha],
                },
                &[range],
            );
            raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(self.image)
                    .subresource_range(range)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
            );
            raw.end_command_buffer(command).unwrap();
            let waits = returned.into_iter().collect::<Vec<_>>();
            let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
            let wait_values = vec![returned_value; waits.len()];
            let signals = [self.ready];
            let signal_values = [value];
            let commands = [command];
            let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wait_values)
                .signal_semaphore_values(&signal_values);
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&commands)
                .signal_semaphores(&signals)
                .push_next(&mut timeline);
            let fence = raw.create_fence(&vk::FenceCreateInfo::default(), None).unwrap();
            raw.queue_submit(raw.get_device_queue(family, 0), &[submit], fence)
                .unwrap();
            raw.wait_for_fences(&[fence], true, 5_000_000_000).unwrap();
            raw.destroy_fence(fence, None);
            raw.free_command_buffers(self.pool, &commands);
            for semaphore in waits {
                raw.destroy_semaphore(semaphore, None);
            }
        }
        self.last = value;
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        unsafe {
            let hal = self.owner.raw_device();
            let raw = hal.raw_device();
            raw.device_wait_idle().unwrap();
            raw.destroy_command_pool(self.pool, None);
            raw.destroy_semaphore(self.ready, None);
            raw.destroy_image(self.image, None);
            raw.free_memory(self.memory, None);
        }
    }
}

struct Notice(mpsc::Sender<()>);
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self(self.0.clone()))
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
        let _ = self.0.send(());
    }
}
pub struct RenderFixture {
    renderer: Option<webrender::Renderer>,
    api: RenderApi,
    document: DocumentId,
    ready: mpsc::Receiver<()>,
    old_device:Option<std::sync::Weak<Device>>,
}
impl RenderFixture {
    fn new() -> Self {
        let (tx, ready) = mpsc::channel();
        let (renderer, sender) = webrender::create_webrender_instance(
            webrender::GpuBackendConfig::Vulkan(Options {
                validation: true,
                ..Default::default()
            }),
            Box::new(Notice(tx)),
            webrender::WebRenderOptions {
                enable_debugger: false,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let api = sender.create_api();
        let document = api.add_document(DeviceIntSize::new(4, 4));
        Self {
            renderer: Some(renderer),
            api,
            document,
            ready,
            old_device:None,
        }
    }
    fn sample(&mut self, handle: ExternalTextureHandle) {
        struct Handler(ExternalTextureHandle);
        impl ExternalImageHandler for Handler {
            fn lock(&mut self, _: ExternalImageId, _: u8, _: bool) -> ExternalImage<'_> {
                ExternalImage {
                    uv: TexelRect::new(0., 0., 4., 4.),
                    source: ExternalImageSource::NativeTexture(self.0),
                }
            }
            fn unlock(&mut self, _: ExternalImageId, _: u8) {}
        }
        self.renderer
            .as_mut()
            .unwrap()
            .set_external_image_handler(Box::new(Handler(handle)));
        self.publish_frame();
        let renderer = self.renderer.as_mut().unwrap();
        renderer.update();
        assert!(
            !renderer
                .render(DeviceIntSize::new(4, 4), 0)
                .unwrap()
                .external_images_pending
        );
        assert_eq!(
            renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(4, 4))),
            [255, 0, 0, 255].repeat(16)
        );
    }
    fn publish_frame(&mut self) {
        let pipeline = PipelineId(0, 0);
        let mut transaction = Transaction::new();
        let key = self.api.generate_image_key();
        transaction.add_image(
            key,
            ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE),
            ImageData::External(ExternalImageData {
                id: ExternalImageId(1),
                channel_index: 0,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            }),
            None,
        );
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let rect = LayoutRect::from_size(LayoutSize::new(4., 4.));
        builder.push_image(
            &CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline)),
            rect,
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            key,
            ColorF::WHITE,
        );
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(0), self.api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
        self.ready.recv_timeout(Duration::from_secs(15)).unwrap();
    }
}
impl Drop for RenderFixture {
    fn drop(&mut self) {
        self.api.delete_document(self.document);
        if let Some(renderer) = self.renderer.take() {
            renderer.deinit();
        }
    }
}
pub struct Fixture {
    producer: Producer,
    render: RenderFixture,
}
#[no_mangle]
pub extern "C" fn wr_test_native_new(output: &mut TestNativeImage) -> *mut Fixture {
    wr_test_native_new_alpha(output, 255)
}
#[no_mangle]
pub extern "C" fn wr_test_native_new_alpha(output: &mut TestNativeImage, alpha: u8) -> *mut Fixture {
    let mut producer = Producer::new();
    producer.alpha = f32::from(alpha) / 255.;
    producer.publish(1, -1, 0);
    *output = TestNativeImage {
        memory_fd: producer.memory_fd.as_raw_fd(),
        ready_fd: producer.ready_fd.as_raw_fd(),
        offset: producer.offset,
        stride: producer.stride,
        device_uuid: producer.device_uuid,
        driver_uuid: producer.driver_uuid,
    };
    Box::into_raw(Box::new(Fixture {
        producer,
        render: RenderFixture::new(),
    }))
}
#[no_mangle]
pub extern "C" fn wr_test_native_renderer(fixture: &mut Fixture) -> *mut webrender::Renderer {
    fixture.render.renderer.as_mut().unwrap()
}
#[no_mangle]
pub extern "C" fn wr_test_native_sample(fixture: &mut Fixture, handle: ExternalTextureHandle) {
    fixture.render.sample(handle);
}
#[no_mangle]
pub extern "C" fn wr_test_native_republish(fixture: &mut Fixture, fd: i32, value: u64) {
    fixture.producer.publish(2, fd, value);
}
#[no_mangle]
pub extern "C" fn wr_test_native_drop_renderer(fixture: &mut Fixture) {
    fixture.render.api.delete_document(fixture.render.document);
    fixture.render.renderer.take().unwrap().deinit();
}
#[no_mangle]
pub extern "C" fn wr_test_native_replace_renderer(fixture: &mut Fixture) {
    fixture.render = RenderFixture::new();
}
#[no_mangle]
pub unsafe extern "C" fn wr_test_native_delete(fixture: *mut Fixture) {
    drop(Box::from_raw(fixture));
}

#[repr(C)]
pub struct TestLock {
    pub handle: u64,
    pub state: u32,
}
struct Callbacks {
    context: *mut std::ffi::c_void,
    lock: extern "C" fn(*mut std::ffi::c_void) -> TestLock,
    unlock: extern "C" fn(*mut std::ffi::c_void),
}
impl ExternalImageHandler for Callbacks {
    fn lock(&mut self, _: ExternalImageId, _: u8, _: bool) -> ExternalImage<'_> {
        let result = (self.lock)(self.context);
        ExternalImage {
            uv: TexelRect::new(0., 0., 4., 4.),
            source: match result.state {
                0 => ExternalImageSource::Pending,
                1 => ExternalImageSource::NativeTexture(ExternalTextureHandle(result.handle)),
                _ => ExternalImageSource::Invalid,
            },
        }
    }
    fn unlock(&mut self, _: ExternalImageId, _: u8) {
        (self.unlock)(self.context);
    }
}
#[no_mangle]
pub extern "C" fn wr_test_native_publish(
    fixture: &mut Fixture,
    context: *mut std::ffi::c_void,
    lock: extern "C" fn(*mut std::ffi::c_void) -> TestLock,
    unlock: extern "C" fn(*mut std::ffi::c_void),
) {
    fixture
        .render
        .renderer
        .as_mut()
        .unwrap()
        .set_external_image_handler(Box::new(Callbacks { context, lock, unlock }));
    fixture.render.publish_frame();
}

#[no_mangle]
pub extern "C" fn wr_test_native_open_ready(fixture: &mut Fixture) {
    let hal = fixture.producer.owner.raw_device();
    let raw = hal.raw_device();
    let signals = [fixture.producer.ready];
    let values = [2];
    let mut timeline = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
    unsafe {
        let fence = raw.create_fence(&vk::FenceCreateInfo::default(), None).unwrap();
        raw.queue_submit(
            raw.get_device_queue(hal.queue_family_index(), 0),
            &[vk::SubmitInfo::default()
                .signal_semaphores(&signals)
                .push_next(&mut timeline)],
            fence,
        )
        .unwrap();
        raw.wait_for_fences(&[fence], true, 5_000_000_000).unwrap();
        raw.destroy_fence(fence, None);
    }
}

impl RenderFixture {
    fn sample_expected(&mut self, handle:ExternalTextureHandle, size:DeviceIntSize, expected:&[u8]) {
        struct Handler(ExternalTextureHandle,DeviceIntSize);
        impl ExternalImageHandler for Handler {
            fn lock(&mut self,_:ExternalImageId,_:u8,_:bool)->ExternalImage<'_>{ExternalImage{uv:TexelRect::new(0.,0.,self.1.width as f32,self.1.height as f32),source:ExternalImageSource::NativeTexture(self.0)}}
            fn unlock(&mut self,_:ExternalImageId,_:u8){}
        }
        self.renderer.as_mut().unwrap().set_external_image_handler(Box::new(Handler(handle,size)));
        let pipeline=PipelineId(0,0);let key=self.api.generate_image_key();let mut transaction=Transaction::new();
        transaction.set_document_view(DeviceIntRect::from_size(size));
        transaction.add_image(key,ImageDescriptor::new(size.width,size.height,ImageFormat::RGBA8,ImageDescriptorFlags::empty()),ImageData::External(ExternalImageData{id:ExternalImageId(1),channel_index:0,image_type:ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),normalized_uvs:false}),None);
        let mut builder=DisplayListBuilder::new(pipeline);builder.begin(60.);let rect=LayoutRect::from_size(LayoutSize::new(size.width as f32,size.height as f32));let common=CommonItemProperties::new(rect,SpaceAndClipInfo::root_scroll(pipeline));builder.push_rect(&common,rect,ColorF::new(1.,0.,0.,1.));builder.push_image(&common,rect,ImageRendering::Pixelated,AlphaType::PremultipliedAlpha,key,ColorF::WHITE);
        transaction.set_root_pipeline(pipeline);transaction.set_display_list(Epoch(0),self.api.get_namespace_id(),builder.end());transaction.generate_frame(1,true,false,RenderReasons::TESTING);self.api.send_transaction(self.document,transaction);self.ready.recv_timeout(Duration::from_secs(15)).unwrap();
        let renderer=self.renderer.as_mut().unwrap();renderer.update();assert!(!renderer.render(size,0).unwrap().external_images_pending);
        let pixels=renderer.read_pixels_rgba8(FramebufferIntRect::from_size(size.cast_unit()));assert_eq!(pixels.len(),expected.len());
        for(index,(actual,expected))in pixels.chunks_exact(4).zip(expected.chunks_exact(4)).enumerate(){assert_eq!(actual[3],expected[3],"alpha pixel={}",index);assert!(actual[..3].iter().zip(&expected[..3]).all(|(&a,&b)|a.abs_diff(b)<=1),"pixel={} actual={:?} expected={:?}",index,actual,expected);}
    }
}




#[no_mangle]
pub extern "C" fn wr_test_foreign_new() -> *mut RenderFixture {
    Box::into_raw(Box::new(RenderFixture::new()))
}

#[no_mangle]
pub extern "C" fn wr_test_foreign_renderer(
    fixture: &mut RenderFixture,
) -> *mut webrender::Renderer {
    fixture.renderer.as_mut().unwrap()
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_sample_expected(
    fixture: &mut RenderFixture,
    handle: ExternalTextureHandle,
    width: i32,
    height: i32,
    expected: *const u8,
    length: usize,
) {
    fixture.sample_expected(
        handle,
        DeviceIntSize::new(width, height),
        std::slice::from_raw_parts(expected, length),
    );
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_delete(fixture: *mut RenderFixture) {
    drop(Box::from_raw(fixture));
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_foreign_replace(fixture:*mut RenderFixture)->*mut RenderFixture {
    let old=Box::from_raw(fixture);
    let weak=Arc::downgrade(old.renderer.as_ref().unwrap().wgpu_external_textures().unwrap().device());
    drop(old);
    let mut replacement=RenderFixture::new();replacement.old_device=Some(weak);
    Box::into_raw(Box::new(replacement))
}
#[no_mangle]
pub extern "C" fn wr_test_foreign_advance(fixture:&RenderFixture) {
    let registry=fixture.renderer.as_ref().unwrap().wgpu_external_textures().unwrap();
    for _ in 0..10 {
        let encoder=registry.device().create_command_encoder("replacement independent work").unwrap();
        registry.device().submit(&[encoder.finish(&Default::default())]).unwrap();registry.device().poll().unwrap();
    }
}
#[no_mangle]
pub extern "C" fn wr_test_foreign_old_device_alive(fixture:&RenderFixture)->bool {
    fixture.old_device.as_ref().is_some_and(|device|device.upgrade().is_some())
}
