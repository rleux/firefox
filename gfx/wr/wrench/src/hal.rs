/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub fn dispatch(args: &clap::ArgMatches) -> Option<i32> {
    let requested = args.value_of("backend") == Some("hal");
    let result = if requested {
        run(args)
    } else if args.value_of("hal_backend").is_some()
        || args.value_of("hal_adapter").is_some()
        || args.is_present("hal_validation")
        || args.subcommand_name() == Some("test_hal")
    {
        Err("HAL options and test_hal require --backend hal".into())
    } else {
        return None;
    };
    Some(match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("HAL error: {error}");
            1
        }
    })
}

#[cfg(all(test, feature = "hal-vulkan"))]
mod tests {
    #[test]
    #[ignore = "Requires a Vulkan ICD and validation layer"]
    fn retained_image_updates_and_resize() {
        #[cfg(feature = "env_logger")]
        let _ = env_logger::builder().is_test(true).try_init();
        use webrender::api::*;
        use webrender::api::units::*;
        use webrender::render_api::Transaction;
        use crate::wrench::Wrench;
        let mut wrench = Wrench::new_hal(
            &webrender::hal::Options {
                validation: true,
                ..Default::default()
            },
            DeviceIntSize::new(257, 129),
        )
        .unwrap();
        let pipeline = wrench.root_pipeline_id;
        let image = wrench.api.generate_image_key();
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(crate::AU_PER_DEV_PX);
        let info = CommonItemProperties {
            clip_rect: LayoutRect::from_size(LayoutSize::new(257.0, 129.0)),
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
        builder.push_image(
            &info,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(7.0, 9.0),
                LayoutSize::new(63.0, 47.0),
            ),
            ImageRendering::Pixelated,
            AlphaType::PremultipliedAlpha,
            image,
            ColorF::WHITE,
        );
        builder.push_rect(
            &info,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(170.0, 13.0),
                LayoutSize::new(60.0, 90.0),
            ),
            ColorF::new(0.0, 0.0, 1.0, 1.0),
        );
        builder.pop_stacking_context();
        let mut display_list = Some(builder.end());
        let mut previous = None;
        for (index, (width, height, color)) in [
            (8, 8, [255, 0, 0, 255]),
            (8, 8, [0, 255, 0, 255]),
            (13, 7, [0, 0, 255, 255]),
            (13, 7, [0, 0, 255, 255]),
        ]
        .iter()
        .enumerate()
        {
            let mut transaction = Transaction::new();
            let descriptor = ImageDescriptor::new(
                *width,
                *height,
                ImageFormat::RGBA8,
                ImageDescriptorFlags::IS_OPAQUE,
            );
            let data = ImageData::new(color.repeat((*width * *height) as usize));
            if let Some(list) = display_list.take() {
                transaction.add_image(image, descriptor, data, None);
                transaction.set_display_list(Epoch(0), wrench.api.get_namespace_id(), list);
            } else if index != 3 {
                transaction.update_image(image, descriptor, data, &DirtyRect::All);
            }
            transaction.generate_frame(index as u64, true, false, RenderReasons::TESTING);
            wrench.begin_frame();
            wrench.api.send_transaction(wrench.document_id, transaction);
            wrench.renderer.prepare_frame(wrench.document_id).unwrap();
            let frame = wrench.renderer.render_frame().unwrap();
            let pixel =
                |x: usize, y: usize| &frame.pixels[(y * 257 + x) * 4..(y * 257 + x + 1) * 4];
            assert_eq!(pixel(20, 20), color);
            assert_eq!(pixel(180, 30), [0, 0, 255, 255]);
            assert_eq!(pixel(250, 120), [255, 255, 255, 255]);
            if index == 3 {
                assert_eq!(previous.as_ref().unwrap(), &frame.pixels);
            }
            previous = Some(frame.pixels);
            println!("HAL image update frame {index} passed");
        }
        wrench.api.shut_down(true);
    }

    #[test]
    #[ignore = "Requires Vulkan and the pinned Linux font environment"]
    fn sustained_resource_reuse() {
        #[cfg(feature = "env_logger")]
        let _ = env_logger::builder().is_test(true).try_init();
        use crate::wrench::Wrench;
        use webrender::api::*;
        use webrender::api::units::*;
        use webrender::render_api::{Transaction, DebugCommand, ClearCache};
        struct State {
            wrench: Wrench<webrender::hal::Renderer>,
            image: ImageKey,
            font: FontInstanceKey,
            glyphs: Vec<u32>,
            color: [u8; 4],
            previous: Option<Vec<u8>>,
        }
        let mut states = Vec::new();
        for _ in 0..2 {
            let mut wrench = Wrench::new_hal(
                &webrender::hal::Options {
                    validation: true,
                    ..Default::default()
                },
                DeviceIntSize::new(257, 129),
            )
            .unwrap();
            let font_key = wrench.font_key_from_bytes(
                std::fs::read(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("reftests/text/FreeSans.ttf"),
                )
                .unwrap(),
                0,
            );
            let glyphs = wrench
                .api
                .get_glyph_indices(font_key, "Reuse 0123456789")
                .into_iter()
                .map(Option::unwrap)
                .collect();
            let font = wrench.api.generate_font_instance_key();
            let mut transaction = Transaction::new();
            transaction.add_font_instance(
                font,
                font_key,
                18.0,
                Some(FontInstanceOptions {
                    render_mode: FontRenderMode::Alpha,
                    ..Default::default()
                }),
                None,
                Vec::new(),
            );
            wrench.api.send_transaction(wrench.document_id, transaction);
            let image = wrench.api.generate_image_key();
            states.push(State {
                wrench,
                image,
                font,
                glyphs,
                color: [255, 0, 0, 255],
                previous: None,
            });
        }
        let mut observed_pending = false;
        let mut samples = Vec::new();
        for index in 0..1100 {
            let which = index % 2;
            let serial = index / 2;
            let state = &mut states[which];
            let wrench = &mut state.wrench;
            let size = DeviceIntSize::new(257 + if serial / 40 % 2 == 0 { 0 } else { 7 }, 129);
            let replace = serial % 100 == 0;
            let rebuild = serial % 5 == 0 || replace;
            let update = serial % 4 != 0 || replace;
            let present = serial % 31 != 0;
            let mut transaction = Transaction::new();
            if serial % 100 == 99 {
                wrench
                    .api
                    .send_debug_cmd(DebugCommand::ClearCaches(ClearCache::all()));
            }
            if update {
                if serial % 2 == 0 {
                    state.color = [255, 0, 0, 255];
                } else {
                    state.color = [0, 255, 0, 255];
                }
                let width = if serial / 17 % 2 == 0 { 8 } else { 13 };
                let descriptor = ImageDescriptor::new(
                    width,
                    7,
                    ImageFormat::RGBA8,
                    ImageDescriptorFlags::IS_OPAQUE,
                );
                let data = ImageData::new(state.color.repeat(width as usize * 7));
                if replace {
                    if serial != 0 {
                        transaction.delete_image(state.image);
                        state.image = wrench.api.generate_image_key();
                    }
                    transaction.add_image(state.image, descriptor, data, None);
                } else {
                    transaction.update_image(state.image, descriptor, data, &DirtyRect::All);
                }
            }
            if rebuild {
                transaction.set_document_view(DeviceIntRect::from_size(size));
                let pipeline = wrench.root_pipeline_id;
                let spatial = SpatialId::root_scroll_node(pipeline);
                let mut builder = DisplayListBuilder::new(pipeline);
                builder.begin(crate::AU_PER_DEV_PX);
                let full =
                    LayoutRect::from_size(LayoutSize::new(size.width as f32, size.height as f32));
                let common = CommonItemProperties {
                    clip_rect: full,
                    clip_chain_id: ClipChainId::INVALID,
                    spatial_id: spatial,
                    flags: PrimitiveFlags::default(),
                };
                let rect = |x, y, w, h| {
                    LayoutRect::from_origin_and_size(LayoutPoint::new(x, y), LayoutSize::new(w, h))
                };
                builder.push_stacking_context(
                    spatial,
                    common.flags,
                    None,
                    TransformStyle::Flat,
                    MixBlendMode::Normal,
                    &[],
                    &[],
                    RasterSpace::Screen,
                    StackingContextFlags::empty(),
                    None,
                );
                builder.push_image(
                    &common,
                    rect(7.0, 9.0, 63.0, 47.0),
                    ImageRendering::Pixelated,
                    AlphaType::PremultipliedAlpha,
                    state.image,
                    ColorF::WHITE,
                );
                builder.push_rect(
                    &common,
                    rect(170.0, 13.0, 60.0, 70.0),
                    ColorF::new(0.0, 0.0, 1.0, 1.0),
                );
                let clip = builder.define_clip_rounded_rect(
                    spatial,
                    ComplexClipRegion::new(
                        rect(85.0, 10.0, 60.0, 50.0),
                        BorderRadius::uniform(8.0 + (serial % 7) as f32),
                        Default::default(),
                        ClipMode::Clip,
                    ),
                );
                let chain = builder.define_clip_chain(None, std::iter::once(clip));
                let clipped = CommonItemProperties {
                    clip_chain_id: chain,
                    ..common
                };
                builder.push_stacking_context(
                    spatial,
                    common.flags,
                    None,
                    TransformStyle::Flat,
                    MixBlendMode::Normal,
                    &[FilterOp::Blur(2.0, 2.0, true)],
                    &[],
                    RasterSpace::Screen,
                    StackingContextFlags::empty(),
                    None,
                );
                builder.push_rect(
                    &clipped,
                    rect(85.0, 10.0, 60.0, 50.0),
                    ColorF::new(0.5, 0.0, 0.5, 1.0),
                );
                builder.pop_stacking_context();
                let glyphs: Vec<_> = state
                    .glyphs
                    .iter()
                    .cycle()
                    .skip(serial % state.glyphs.len())
                    .take(12)
                    .enumerate()
                    .map(|(i, &glyph)| GlyphInstance {
                        index: glyph,
                        point: LayoutPoint::new(80.0 + i as f32 * 11.0, 112.0),
                    })
                    .collect();
                builder.push_text(
                    &common,
                    rect(75.0, 86.0, 155.0, 31.0),
                    &glyphs,
                    state.font,
                    ColorF::BLACK,
                    None,
                );
                builder.pop_stacking_context();
                transaction.set_display_list(
                    Epoch(serial as u32),
                    wrench.api.get_namespace_id(),
                    builder.end(),
                );
            }
            transaction.generate_frame(serial as u64, present, false, RenderReasons::TESTING);
            wrench.begin_frame();
            wrench.api.send_transaction(wrench.document_id, transaction);
            let prepared = wrench.renderer.prepare_frame(wrench.document_id).unwrap();
            if index < 6 {
                println!("HAL_STRESS start={index} serial={serial} present={present} {prepared:?}");
            }
            let frame = wrench.renderer.render_frame().unwrap();
            let memory = wrench.renderer.memory_stats();
            observed_pending |= memory.in_flight > 0;
            assert!(memory.in_flight <= 3);
            assert!(memory.cached_buffer_bytes <= 64 * 1024 * 1024);
            assert!(memory.cached_texture_bytes <= 64 * 1024 * 1024);
            assert!(memory.pipelines <= 128 && memory.descriptors <= 256);
            assert_eq!(memory.pending_notifications, 0);
            wrench.renderer.flush_pipeline_info();
            if present {
                assert_eq!(frame.size, [size.width as u32, size.height as u32]);
                let pixel = |x, y| {
                    &frame.pixels
                        [(y * size.width as usize + x) * 4..(y * size.width as usize + x + 1) * 4]
                };
                assert_eq!(pixel(20, 20), state.color, "frame {index} serial {serial}");
                assert_eq!(pixel(180, 30), [0, 0, 255, 255]);
                assert_eq!(pixel(250, 120), [255, 255, 255, 255]);
                if !update && !rebuild {
                    if let Some(previous) = &state.previous {
                        assert_eq!(previous, &frame.pixels);
                    }
                }
                state.previous = Some(frame.pixels);
            } else {
                assert!(frame.pixels.is_empty());
                state.previous = None;
            }
            if index >= 100 && index % 100 < 2 {
                println!("HAL_MEMORY frame={index} renderer={which} {memory:?}");
                samples.push((which, memory.buffer_bytes + memory.texture_bytes));
            }
        }
        assert!(observed_pending);
        for which in 0..2 {
            let values: Vec<_> = samples
                .iter()
                .filter(|(id, _)| *id == which)
                .map(|(_, bytes)| *bytes)
                .collect();
            assert!(values.iter().copied().max().unwrap() < 256 * 1024 * 1024);
            assert!(values.last().unwrap() <= &(values[0] + 32 * 1024 * 1024));
            states[which].wrench.api.shut_down(true);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan"]
    fn owned_yuv_ranges_and_updates() {
        use crate::wrench::Wrench;
        use webrender::api::*;
        use webrender::api::units::*;
        use webrender::render_api::Transaction;
        let mut wrench = Wrench::new_hal(
            &webrender::hal::Options {
                validation: true,
                ..Default::default()
            },
            DeviceIntSize::new(32, 32),
        )
        .unwrap();
        let y = wrench.api.generate_image_key();
        let u = wrench.api.generate_image_key();
        let v = wrench.api.generate_image_key();
        let mut initialized = false;
        let mut serial = 0;
        for space in [
            YuvColorSpace::Rec601,
            YuvColorSpace::Rec709,
            YuvColorSpace::Rec2020,
        ] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                for white in [false, true] {
                    let mut transaction = Transaction::new();
                    let luma = match (range, white) {
                        (ColorRange::Limited, false) => 16,
                        (ColorRange::Limited, true) => 235,
                        (_, false) => 0,
                        (_, true) => 255,
                    };
                    let desc = ImageDescriptor::new(
                        4,
                        4,
                        ImageFormat::R8,
                        ImageDescriptorFlags::IS_OPAQUE,
                    );
                    if initialized {
                        transaction.update_image(
                            y,
                            desc,
                            ImageData::new(vec![luma; 16]),
                            &DirtyRect::All,
                        );
                    } else {
                        transaction.add_image(y, desc, ImageData::new(vec![luma; 16]), None);
                        transaction.add_image(u, desc, ImageData::new(vec![128; 16]), None);
                        transaction.add_image(v, desc, ImageData::new(vec![128; 16]), None);
                        initialized = true;
                    }
                    let pipeline = wrench.root_pipeline_id;
                    let spatial = SpatialId::root_scroll_node(pipeline);
                    let rect = LayoutRect::from_size(LayoutSize::new(32.0, 32.0));
                    let common = CommonItemProperties {
                        clip_rect: rect,
                        clip_chain_id: ClipChainId::INVALID,
                        spatial_id: spatial,
                        flags: PrimitiveFlags::default(),
                    };
                    let mut builder = DisplayListBuilder::new(pipeline);
                    builder.begin(crate::AU_PER_DEV_PX);
                    builder.push_stacking_context(
                        spatial,
                        common.flags,
                        None,
                        TransformStyle::Flat,
                        MixBlendMode::Normal,
                        &[],
                        &[],
                        RasterSpace::Screen,
                        StackingContextFlags::empty(),
                        None,
                    );
                    builder.push_yuv_image(
                        &common,
                        rect,
                        YuvData::PlanarYCbCr(y, u, v),
                        ColorDepth::Color8,
                        space,
                        range,
                        ImageRendering::Auto,
                    );
                    builder.pop_stacking_context();
                    transaction.set_display_list(
                        Epoch(serial),
                        wrench.api.get_namespace_id(),
                        builder.end(),
                    );
                    transaction.generate_frame(serial as u64, true, false, RenderReasons::TESTING);
                    wrench.api.send_transaction(wrench.document_id, transaction);
                    wrench.renderer.prepare_frame(wrench.document_id).unwrap();
                    let output = wrench.renderer.render_frame().unwrap();
                    let pixel = &output.pixels[(16 * 32 + 16) * 4..(16 * 32 + 17) * 4];
                    let expected = if white { 255u8 } else { 0u8 };
                    assert!(
                        pixel[..3].iter().all(|value| value.abs_diff(expected) <= 1),
                        "{space:?} {range:?} white={white}: {pixel:?}"
                    );
                    assert_eq!(pixel[3], 255);
                    serial += 1;
                }
            }
        }
        wrench.api.shut_down(true);
    }

    #[test]
    #[ignore = "Requires a Vulkan ICD and validation layer"]
    fn existing_yaml_builds_a_wr_frame() {
        #[cfg(feature = "env_logger")]
        let _ = env_logger::builder().is_test(true).try_init();
        use crate::wrench::Wrench;
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::DeviceIntSize;
        let mut wrench = Wrench::new_hal(
            &webrender::hal::Options {
                validation: true,
                ..Default::default()
            },
            DeviceIntSize::new(800, 800),
        )
        .unwrap();
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("reftests/image/tile-size.yaml");
        let mut reader = YamlFrameReader::new(&path);
        reader.build_frame(&mut wrench);
        let frame = wrench.renderer.prepare_frame(wrench.document_id).unwrap();
        assert!(frame.passes > 0, "{:?}", frame);
        assert!(frame.primitive_instances > 0, "{:?}", frame);
        println!("HAL prepared current WR frame: {frame:?}");
        wrench.api.shut_down(true);
    }
}

#[cfg(not(feature = "hal-vulkan"))]
fn run(_: &clap::ArgMatches) -> Result<(), String> {
    Err("Vulkan support is not compiled in; build Wrench with --features hal-vulkan".into())
}

#[cfg(feature = "hal-vulkan")]
fn run(args: &clap::ArgMatches) -> Result<(), String> {
    use webrender::hal::{create_vulkan_device, Options, Readback};

    if !args.is_present("headless") {
        return Err("HAL bootstrap requires --headless".into());
    }
    for option in [
        "software",
        "angle",
        "compositor",
        "renderer",
        "precache",
        "shaders",
        "use_unoptimized_shaders",
    ] {
        if args.occurrences_of(option) != 0 {
            return Err(format!("--{option} is incompatible with the HAL bootstrap"));
        }
    }
    let command = args.subcommand_name().unwrap_or("");
    if !matches!(command, "test_init" | "test_hal" | "png" | "reftest") {
        return Err(format!(
            "{command:?} is not implemented for HAL yet; use test_init or test_hal"
        ));
    }
    let dimensions = match args.value_of("size") {
        None if matches!(command, "png" | "reftest") => [1920, 1080],
        None => [7, 5],
        Some("720p") => [1280, 720],
        Some("1080p") => [1920, 1080],
        Some("4k") => [3840, 2160],
        Some(value) => {
            let (w, h) = value
                .split_once('x')
                .ok_or("Invalid size; expected WIDTHxHEIGHT")?;
            [
                w.parse().map_err(|_| "Invalid width")?,
                h.parse().map_err(|_| "Invalid height")?,
            ]
        }
    };
    let options = Options {
        adapter_name: args.value_of("hal_adapter").map(str::to_owned),
        validation: args.is_present("hal_validation"),
    };
    if command == "png" {
        return render_png(args, &options, dimensions);
    }
    if command == "reftest" {
        use crate::reftest::{ReftestHarness, ReftestOptions};
        use crate::wrench::Wrench;
        use std::path::Path;
        let size =
            webrender::api::units::DeviceIntSize::new(dimensions[0] as i32, dimensions[1] as i32);
        let mut wrench =
            Wrench::new_hal_with_subpixel(&options, size, !args.is_present("no_subpixel_aa"))?;
        println!(
            "Backend: wgpu-hal/{:?}; adapter: {}; type: {:?}",
            wrench.renderer.info().backend,
            wrench.renderer.info().name,
            wrench.renderer.info().device_type
        );
        let reftest_args = args.subcommand_matches("reftest").unwrap();
        if reftest_args.value_of("fuzz_tolerance").is_some() {
            return Err("HAL reftests require the existing per-test tolerances".into());
        }
        let filter = reftest_args.value_of("REFTEST").map(Path::new);
        let failures = ReftestHarness::new_hal(&mut wrench, size).run(
            Path::new("reftests/reftest.list"),
            filter,
            &ReftestOptions::default(),
        );
        wrench.api.shut_down(true);
        return if failures == 0 {
            Ok(())
        } else {
            Err(format!("{failures} HAL reftests failed"))
        };
    }
    let mut device = create_vulkan_device(&options)?;
    let info = device.info();
    println!(
        "Backend: wgpu-hal/{:?}; adapter: {}; type: {:?}; vendor: {:#x}; device: {:#x}",
        info.backend, info.name, info.device_type, info.vendor, info.device
    );
    println!(
        "Driver: {} {}; validation requested: {}",
        info.driver, info.driver_info, options.validation
    );
    println!(
        "ICD selection: {:?}",
        std::env::var("VK_DRIVER_FILES")
            .or_else(|_| std::env::var("VK_ICD_FILENAMES"))
            .ok()
    );

    fn check(readback: Readback, color: [u8; 4], depth: f32) -> Result<(), String> {
        let count = readback.size[0] as usize * readback.size[1] as usize;
        if readback.color.len() != count * 4
            || readback.depth.len() != count
            || !readback.color.chunks_exact(4).all(|pixel| pixel == color)
            || !readback.depth.iter().all(|&value| value == depth)
        {
            return Err(format!(
                "Offscreen {:?} color/depth readback mismatch",
                readback.size
            ));
        }
        println!(
            "HAL PASS color/depth readback {}x{}",
            readback.size[0], readback.size[1]
        );
        Ok(())
    }
    check(
        device.clear_and_readback(dimensions[0], dimensions[1], [17, 31, 199, 255], 0.25)?,
        [17, 31, 199, 255],
        0.25,
    )?;
    if command == "test_hal" {
        for (size, color, depth) in [
            ([1, 1], [255, 0, 128, 63], 1.0),
            ([63, 3], [0, 255, 7, 255], 0.0),
            ([17, 9], [63, 71, 89, 0], 0.5),
        ] {
            check(
                device.clear_and_readback(size[0], size[1], color, depth)?,
                color,
                depth,
            )?;
        }
        if device.clear_and_readback(0, 1, [0; 4], 1.0).is_ok()
            || device.clear_and_readback(u32::MAX, 1, [0; 4], 1.0).is_ok()
        {
            return Err("Invalid target dimensions were accepted".into());
        }
        device.test_native_image(7, 5, [71, 39, 211, 255])?;
        device.test_native_image(13, 3, [255, 128, 0, 63])?;
        println!("HAL PASS native-image acquire/read/release (same Vulkan device; not cross-process import)");
    }
    println!("HAL initialization successful; no GL context created");
    Ok(())
}

#[cfg(feature = "hal-vulkan")]
fn render_png(
    args: &clap::ArgMatches,
    options: &webrender::hal::Options,
    size: [u32; 2],
) -> Result<(), String> {
    use crate::wrench::Wrench;
    use crate::yaml_frame_reader::YamlFrameReader;
    use webrender::api::units::DeviceIntSize;
    use std::convert::TryFrom;
    let args = args.subcommand_matches("png").unwrap();
    if args
        .value_of("surface")
        .is_some_and(|surface| surface != "screen")
    {
        return Err("HAL only supports screen PNG output".into());
    }
    let size = DeviceIntSize::new(
        i32::try_from(size[0]).map_err(|_| "Invalid width")?,
        i32::try_from(size[1]).map_err(|_| "Invalid height")?,
    );
    let mut wrench =
        Wrench::new_hal_with_subpixel(options, size, !args.is_present("no_subpixel_aa"))?;
    let info = wrench.renderer.info();
    println!(
        "Backend: wgpu-hal/{:?}; adapter: {}; type: {:?}; driver: {} {}",
        info.backend, info.name, info.device_type, info.driver, info.driver_info
    );
    let input = std::path::PathBuf::from(args.value_of("INPUT").unwrap());
    let output = args
        .value_of("OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| input.with_extension("png"));
    let mut reader = YamlFrameReader::new(&input);
    reader.build_frame(&mut wrench);
    let prepared = wrench.renderer.prepare_frame(wrench.document_id)?;
    println!("HAL prepared WR frame: {prepared:?}");
    let frame = wrench.renderer.render_frame()?;
    println!("HAL rendered WR frame: {:?}", frame.stats);
    crate::png::save(
        output,
        frame.pixels,
        size,
        crate::png::SaveSettings {
            flip_vertical: false,
            try_crop: true,
        },
    );
    wrench.api.shut_down(true);
    Ok(())
}
