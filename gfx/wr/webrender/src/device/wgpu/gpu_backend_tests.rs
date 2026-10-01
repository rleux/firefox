/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{Device as NativeDevice, Options};
use crate::device::wgpu::tests::{validation_logging, ERRORS};
use crate::renderer::desc;
use std::sync::atomic::Ordering;

#[test]
fn disabled_profiling_does_not_issue_queries_or_invent_samples() {
    let tag = || crate::profiler::GpuProfileTag {
        label: "disabled profiling",
        color: api::ColorF::new(1.0, 1.0, 1.0, 1.0),
    };
    let mut profiler = GpuProfiler::new(Rc::new(DisabledQueries));
    profiler.enable_timers();
    profiler.enable_samplers();
    for frame in 1..=5 {
        let (_, timers, samplers) = profiler.build_samples();
        assert!(timers.is_empty() && samplers.is_empty());
        profiler.begin_frame(GpuFrameId::new(frame));
        drop(profiler.start_timer(tag()));
        let sample = profiler.start_sampler(tag());
        profiler.finish_sampler(sample);
        drop(profiler.start_marker("disabled marker"));
        profiler.place_marker("disabled marker");
        profiler.end_frame();
    }
}

fn setup() -> (wr::Device, Rc<Texture>) {
    validation_logging();
    let owner = Rc::new(
        NativeDevice::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", owner.info());
    let mut native = RenderDevice::new(&owner).unwrap();
    let output = native
        .textures
        .default_target(DeviceIntSize::new(2, 1))
        .unwrap();
    (
        wr::Device {
            backend: Box::new(native),
            pending_state: RenderState::default(),
            #[cfg(debug_assertions)]
            pipeline_bound: false,
        },
        output,
    )
}

fn device_options() -> wr::DeviceOptions {
    wr::DeviceOptions {
        crash_annotator: None,
        resource_override_path: None,
        use_optimized_shaders: false,
        upload_method: wr::UploadMethod::PixelBuffer(wr::VertexUsageHint::Stream),
        batched_upload_threshold: 0,
        cached_programs: None,
        allow_texture_swizzling: false,
        dump_shader_source: None,
        surface_origin_is_top_left: true,
    }
}

#[test]
fn vulkan_construction_rejects_runtime_source_options_before_opening_a_device() {
    for dump in [false, true] {
        let mut options = device_options();
        if dump {
            options.dump_shader_source = Some("ps_clear".into());
        } else {
            options.resource_override_path = Some("unused-shader-directory".into());
        }
        let error = wr::Device::new(wr::GpuBackendConfig::Vulkan(Options::default()), options)
            .err()
            .unwrap();
        assert!(error.contains("shader source"));
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn public_device_constructor_selects_vulkan_and_propagates_adapter_errors() {
    validation_logging();
    let mut device = wr::Device::new(
        wr::GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        device_options(),
    )
    .unwrap();
    eprintln!("Vulkan adapter: {:?}", device.api_info());
    assert_eq!(device.api_info().kind, wr::GraphicsApi::Vulkan);
    device.begin_frame();
    let mut descriptor = pass();
    descriptor.color_load = wr::LoadOp::Clear([0.0, 1.0, 0.0, 1.0]);
    device.begin_render_pass(&descriptor);
    device.end_render_pass(StoreOp::Store);
    device.end_frame();
    assert!(device.failure().is_none());
    let output = device.wgpu_test_output().unwrap();
    assert_eq!(
        output
            .readback(DeviceIntRect::from_size(DeviceIntSize::new(2, 1)))
            .unwrap()
            .wait()
            .unwrap(),
        [0, 255, 0, 255].repeat(2)
    );
    device.begin_frame();
    device.deinit();
    device.end_frame();
    let error = wr::Device::new(
        wr::GpuBackendConfig::Vulkan(Options {
            validation: true,
            adapter_name: Some(" ".into()),
            ..Default::default()
        }),
        device_options(),
    )
    .err()
    .unwrap();
    assert!(error.contains("Adapter name must not be empty"));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

fn pass() -> RenderPassDescriptor {
    RenderPassDescriptor {
        target: DrawTarget::new_default(DeviceIntSize::new(2, 1), true),
        render_area: None,
        color_load: wr::LoadOp::Clear([0.0; 4]),
        depth_load: wr::LoadOp::Load,
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_device_and_upload_pool_render_and_recycle_vulkan_resources() {
    let (mut device, output) = setup();
    device.begin_frame();
    assert_eq!(device.api_info().kind, wr::GraphicsApi::Vulkan);
    let source = device.create_texture(
        ImageBufferKind::Texture2D,
        ImageFormat::RGBA8,
        2,
        1,
        TextureFilter::Nearest,
        None,
    );
    let copied = device.create_texture(
        ImageBufferKind::Texture2D,
        ImageFormat::RGBA8,
        2,
        1,
        TextureFilter::Nearest,
        None,
    );
    let mut program = device.create_program("cs_scale", &["TEXTURE_2D"]).unwrap();
    wr::GpuBackend::link_program(
        &mut *device,
        &mut program,
        &desc::SCALE,
        &[("sColor0", wr::TextureSlot(0))],
    )
    .unwrap();
    let vertices = device.create_buffer(wr::BufferKind::Vertex);
    let mut instances = device.create_buffer(wr::BufferKind::Vertex);
    let vao = device.create_vertex_array(&desc::SCALE, &vertices, Some(&instances), None, 1);
    device.write_buffer(
        &mut instances,
        &[[0.0f32, 0.0, 2.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0]],
        wr::VertexUsageHint::Stream,
    );
    let mut pool = wr::UploadBufferPool::new(&mut device, 256);
    let full = DeviceIntRect::from_size(DeviceIntSize::new(2, 1));
    for frame in 0..2 {
        if frame != 0 {
            device.begin_frame();
        }
        let bytes: Vec<u8> = if frame == 0 {
            vec![255, 0, 0, 255, 0, 0, 255, 255]
        } else {
            vec![0, 255, 0, 255, 255, 255, 0, 255]
        };
        let mut uploader = device.upload_texture(&mut pool);
        uploader.upload(
            &mut device,
            &source,
            full,
            None,
            None,
            bytes.as_ptr(),
            bytes.len(),
        );
        uploader.flush(&mut device);
        pool.end_frame(&mut device);
        device.copy_texture_sub_region(&source, 0, 0, &copied, 0, 0, 2, 1);
        device.begin_render_pass(&pass());
        device.bind_vertex_array(&vao);
        device.bind_texture(wr::TextureSlot(0), &copied, Swizzle::default());
        device.bind_program(&program);
        device.set_uniforms(&program, &Transform3D::ortho(0.0, 2.0, 0.0, 1.0, -1.0, 1.0));
        device.draw_indexed_triangles_instanced_u16(6, 1);
        device.end_render_pass(StoreOp::Store);
        device.end_frame();
        assert!(device.failure().is_none(), "{:?}", device.failure());
        assert_eq!(output.readback(full).unwrap().wait().unwrap(), bytes);
    }
    device.begin_frame();
    let depth = device.create_texture(
        ImageBufferKind::Texture2D,
        ImageFormat::RGBA8,
        2,
        1,
        TextureFilter::Nearest,
        Some(RenderTargetInfo { has_depth: true }),
    );
    assert_eq!(device.depth_targets_memory(), 8);
    device.delete_texture(depth);
    assert_eq!(device.depth_targets_memory(), 0);
    device.delete_texture(source);
    device.delete_texture(copied);
    device.delete_program(program);
    device.delete_vertex_array(vao);
    device.delete_buffer(vertices);
    device.delete_buffer(instances);
    pool.deinit(&mut device);
    device.deinit();
    device.end_frame();
    assert!(device.failure().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn shared_backend_reports_failures_and_allows_cleanup() {
    {
        let (mut device, _) = setup();
        device.begin_frame();
        assert!(device
            .required_upload_size_and_stride(DeviceIntSize::new(-1, 1), ImageFormat::RGBA8)
            .is_err());
        assert!(device.failure().unwrap().contains("upload dimensions"));
        device.deinit();
        device.end_frame();
    }
    let (mut device, _) = setup();
    device.begin_frame();
    let mut unlinked = device.create_program("cs_scale", &["TEXTURE_2D"]).unwrap();
    let texture = device.create_texture(
        ImageBufferKind::Texture2D,
        ImageFormat::RGBA8,
        2,
        1,
        TextureFilter::Nearest,
        None,
    );
    assert!(device.failure().is_none());
    let capture = device.create_transfer_buffer_with_size(16);
    assert_eq!(capture.get_reserved_size(), 16);
    let failure = device.failure().unwrap().to_owned();
    assert!(failure.contains("capture readback"));
    assert!(wr::GpuBackend::link_program(&mut *device, &mut unlinked, &desc::SCALE, &[]).is_err());
    assert_eq!(unlinked.id, 0);
    device.delete_program(unlinked);
    assert!(!device.take_out_of_memory_error());
    assert!(device.map_transfer_buffer(&capture).is_none());
    let vertices = device.create_buffer(wr::BufferKind::Vertex);
    let mut instances = device.create_buffer(wr::BufferKind::Vertex);
    let vao = device.create_vertex_array(&desc::SCALE, &vertices, Some(&instances), None, 1);
    assert_eq!(vao.instance_stride(), 36);
    let mut upload = device.create_transfer_buffer();
    assert!(device
        .allocate_upload_buffer(&mut upload, 128, wr::VertexUsageHint::Stream, true)
        .is_err());
    assert_eq!(device.failure(), Some(failure.as_str()));
    device.delete_transfer_buffer(upload);
    device.delete_transfer_buffer(capture);
    device.delete_vertex_array(vao);
    device.delete_buffer(vertices);
    device.delete_buffer(instances);
    device.delete_texture(texture);
    assert_eq!(device.textures_deleted(), 1);
    device.deinit();
    device.end_frame();
    assert_eq!(device.failure(), Some(failure.as_str()));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
