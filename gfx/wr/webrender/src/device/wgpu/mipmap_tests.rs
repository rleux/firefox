/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn texture(ctx: &Context, width: u32, height: u32, format: wgt::TextureFormat) -> Rc<Texture> {
    Texture::new(
        &ctx.device,
        width,
        height,
        format,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap()
}

fn generate(
    ctx: &mut Context,
    blitter: &TextureBlitter,
    texture: &Rc<Texture>,
) -> Result<(), String> {
    blitter.generate_mipmaps(
        &mut ctx.queue.recording()?,
        &ctx.queue,
        &mut ctx.scratch,
        texture,
    )
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mipmap_generation_downsamples_each_level_and_supports_trilinear_reads() {
    let mut ctx = Context::new();
    let blitter = ctx.blitter(wgt::TextureFormat::Rgba8Unorm);
    let image = texture(&ctx, 8, 4, wgt::TextureFormat::Rgba8Unorm);
    let base: Vec<_> = (0..4)
        .flat_map(|y| {
            (0..8).flat_map(move |x| {
                let value = (x / 2 * 32 + y / 2 * 128) as u8;
                [value, value, value, 255]
            })
        })
        .collect();
    ctx.upload(&image, &base);
    generate(&mut ctx, &blitter, &image).unwrap();
    assert!(image.sample_initialized());
    assert_eq!(ctx.scratch.bytes(), 0);
    assert!(!ctx.queue.has_pending_work());

    let target = ctx.texture(1, 1, image.format(), true);
    let pass = DrawPass {
        target: &target,
        origin: DeviceIntPoint::zero(),
        viewport: None,
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    };
    let projection = pass.projection(&Transform3D::ortho(0.0, 1.0, 0.0, 1.0, -1.0, 1.0));
    let values = [0.0f32, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0];
    let bytes: Vec<_> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let textures = [(image.clone(), TextureFilter::Trilinear)];
    pass.record_batches(
        &mut ctx.queue.recording().unwrap(),
        &ctx.queue,
        &ctx.quad,
        Some(&ctx.samplers),
        &[DrawBatch {
            pipeline: &blitter.pipeline,
            projection: Some(&projection),
            textures: &textures,
            buffers: &[],
            instances: &bytes,
            instance_count: 1,
            scissor: full(&target),
        }],
    )
    .unwrap();
    assert_eq!(ctx.queue.wait().unwrap(), 1);
    assert_eq!(pixels(&image), base);
    for (level, values) in [
        (1, vec![0u8, 32, 64, 96, 128, 160, 192, 224]),
        (2, vec![80, 144]),
        (3, vec![112]),
    ] {
        let expected: Vec<_> = values.into_iter().flat_map(|v| [v, v, v, 255]).collect();
        close(&pixels(&image.mip_view(level).unwrap()), &expected);
    }
    close(&pixels(&target), &[112, 112, 112, 255]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mipmap_generation_handles_odd_dimensions_formats_and_updates() {
    let mut ctx = Context::new();
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
        wgt::TextureFormat::R8Unorm,
    ] {
        let blitter = ctx.blitter(format);
        let channels = if format == wgt::TextureFormat::R8Unorm {
            1
        } else {
            4
        };
        for (width, height) in [(17, 9), (1, 9), (9, 1), (1, 1)] {
            let image = texture(&ctx, width, height, format);
            let count = (width * height) as usize;
            for value in [64, 192] {
                ctx.upload(&image, &vec![value; count * channels]);
                generate(&mut ctx, &blitter, &image).unwrap();
                ctx.queue.wait().unwrap();
                assert!(image.sample_initialized());
                for level in 0..image.mip_count() {
                    let view = if image.mip_count() == 1 {
                        image.clone()
                    } else {
                        image.mip_view(level).unwrap()
                    };
                    let size = view.size();
                    assert_eq!(size.width, (width >> level).max(1));
                    assert_eq!(size.height, (height >> level).max(1));
                    assert_eq!(
                        pixels(&view),
                        vec![value; (size.width * size.height) as usize * channels]
                    );
                }
            }
        }
    }
    assert_eq!(ctx.scratch.bytes(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mipmap_generation_validates_and_rolls_back_abandoned_chains() {
    let mut ctx = Context::new();
    let blitter = ctx.blitter(wgt::TextureFormat::Rgba8Unorm);
    let image = texture(&ctx, 8, 4, wgt::TextureFormat::Rgba8Unorm);
    assert!(generate(&mut ctx, &blitter, &image)
        .unwrap_err()
        .contains("uninitialized"));
    assert!(!image.sample_initialized());
    let single = texture(&ctx, 1, 1, image.format());
    generate(&mut ctx, &blitter, &single).unwrap();
    assert!(!single.initialized());
    let wrong_format = texture(&ctx, 4, 4, wgt::TextureFormat::R8Unorm);
    assert!(generate(&mut ctx, &blitter, &wrong_format)
        .unwrap_err()
        .contains("format"));
    let foreign = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let foreign_texture = Texture::new(
        &foreign,
        4,
        4,
        image.format(),
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    assert!(generate(&mut ctx, &blitter, &foreign_texture).is_err());
    ctx.upload(&image, &[255, 0, 0, 255].repeat(32));
    ctx.queue.wait().unwrap();
    generate(&mut ctx, &blitter, &image).unwrap();
    assert!(image.sample_initialized());
    ctx.queue.discard_recording();
    assert!(image.initialized());
    for level in 1..image.mip_count() {
        assert!(!image.mip_view(level).unwrap().initialized());
    }
    generate(&mut ctx, &blitter, &image).unwrap();
    ctx.queue.wait().unwrap();
    ctx.upload(&image, &[0, 0, 255, 255].repeat(32));
    generate(&mut ctx, &blitter, &image).unwrap();
    ctx.queue.discard_recording();
    for level in 0..image.mip_count() {
        let view = image.mip_view(level).unwrap();
        assert!(view.initialized());
        assert_eq!(
            pixels(&view),
            [255, 0, 0, 255].repeat((view.size().width * view.size().height) as usize)
        );
    }
    assert_eq!(ctx.scratch.bytes(), 0);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mipmap_chains_share_one_uniform_arena_per_recording() {
    use super::super::super::tests::Command;
    let mut ctx = Context::new();
    let blitter = ctx.blitter(wgt::TextureFormat::Rgba8Unorm);
    let first = texture(&ctx, 1024, 1, blitter.pipeline.format);
    let second = texture(&ctx, 1024, 1, blitter.pipeline.format);
    let colors = [[32, 64, 96, 255], [192, 128, 64, 255]];
    for (image, color) in [&first, &second].iter().zip(colors) {
        ctx.upload(image, &color.repeat(1024));
    }
    ctx.device.trace.borrow_mut().clear();
    generate(&mut ctx, &blitter, &first).unwrap();
    assert!(ctx
        .queue
        .recording()
        .unwrap()
        .with_binding_cache(|commands, cache| {
            cache.resolve(
                commands,
                &ctx.queue,
                &blitter.pipeline,
                Some(&[0.0; 16]),
                &[],
                &[],
                Some(&ctx.samplers),
            )
        })
        .is_err());
    generate(&mut ctx, &blitter, &second).unwrap();
    {
        let trace = ctx.device.trace.borrow();
        let arenas: Vec<_> = trace
            .iter()
            .filter_map(|command| match command {
                Command::UniformArena(bytes) => Some(*bytes),
                _ => None,
            })
            .collect();
        assert_eq!(arenas, [65536]);
        assert_eq!(
            trace
                .iter()
                .filter(|command| matches!(command, Command::BeginPass(..)))
                .count(),
            20
        );
        assert_eq!(
            trace
                .iter()
                .filter(|command| matches!(command, Command::EndPass))
                .count(),
            20
        );
    }
    ctx.queue.wait().unwrap();
    for (image, color) in [&first, &second].iter().zip(colors) {
        for level in 1..image.mip_count() {
            let mip = image.mip_view(level).unwrap();
            assert_eq!(pixels(&mip), color.repeat(mip.size().width as usize));
        }
    }
    ctx.device.trace.borrow_mut().clear();
    generate(&mut ctx, &blitter, &first).unwrap();
    ctx.queue.wait().unwrap();
    assert_eq!(
        ctx.device
            .trace
            .borrow()
            .iter()
            .filter(|command| matches!(command, Command::UniformArena(_)))
            .count(),
        1
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
