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
        || args.value_of("hal_filtering").is_some()
        || args.value_of("hal_compositor").is_some()
        || args.value_of("hal_frames").is_some()
        || args.value_of("hal_windows").is_some()
        || matches!(args.subcommand_name(), Some("test_hal" | "test_surface"))
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
    fn capture_barrier(wrench: &mut crate::wrench::Wrench<webrender::hal::Renderer>) {
        wrench.api.flush_scene_builder();
        let (tx, rx) = webrender::api::channel::unbounded_channel();
        wrench.api.send_debug_cmd(webrender::render_api::DebugCommand::GetDebugFlags(tx));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            wrench.renderer.update().unwrap();
            if rx.try_recv().is_ok() { return; }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn compositor_clip_option_parsing() {
        for (value, expected) in [(None, Ok(None)), (Some("true"), Ok(Some(true))), (Some("false"), Ok(Some(false))), (Some("invalid"), Err(()))] {
            let app = clap::App::new("test").arg(clap::Arg::with_name("compositor_clips").long("compositor-clips").takes_value(true));
            let mut arguments = vec!["test"];
            if let Some(value) = value { arguments.extend(["--compositor-clips", value]); }
            let matches = app.get_matches_from(arguments);
            assert_eq!(super::compositor_clips_override(&matches).map_err(|_| ()), expected);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn compositor_clip_override_survives_harness_default() {
        use crate::wrench::Wrench;
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::DeviceIntSize;
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        let render = |enabled, override_default| {
            let mut wrench = Wrench::new_hal(&options, DeviceIntSize::new(320, 320)).unwrap();
            if override_default {
                wrench.set_compositor_clips_override(enabled);
                wrench.set_compositor_clips_enabled(!enabled);
            } else {
                wrench.set_compositor_clips_enabled(enabled);
            }
            let mut reader = YamlFrameReader::new(std::path::Path::new("reftests/clip/sc-mask-with-blur.yaml"));
            reader.build_frame(&mut wrench);
            wrench.renderer.prepare_frame(wrench.document_id).unwrap();
            let frame = wrench.renderer.render_frame().unwrap();
            wrench.api.shut_down(true);
            (frame.pixels, frame.stats.wr_draw_calls, frame.stats.color_targets)
        };
        let enabled = render(true, false);
        let disabled = render(false, false);
        assert!(enabled != disabled);
        assert!(render(true, true) == enabled);
        assert!(render(false, true) == disabled);
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn filtering_profiles_and_capture() {
        use crate::wrench::Wrench;
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::DeviceIntSize;
        use webrender::hal::Filtering;
        use webrender::render_api::CaptureBits;
        let size = DeviceIntSize::new(350, 90);
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        let policies = [Filtering::Standard, Filtering::LegacyBrilinear];
        let mut renderers: Vec<_> = policies.iter().map(|policy| {
            let mut wrench = Wrench::new_hal(&options, size).unwrap();
            wrench.renderer.configure_filtering(*policy).unwrap();
            assert!(wrench.renderer.configure_filtering(*policy).is_err());
            wrench
        }).collect();
        let root = std::env::temp_dir().join(format!("wr-hal-filtering-{}", std::process::id()));
        for mipmaps in [false, true] {
            let mut outputs = Vec::new();
            for (index, wrench) in renderers.iter_mut().enumerate() {
                let mut reader = YamlFrameReader::new(std::path::Path::new("reftests/image/downscale.yaml"));
                reader.allow_mipmaps(mipmaps);
                reader.build_frame(wrench);
                wrench.renderer.prepare_frame(wrench.document_id).unwrap();
                let pixels = wrench.renderer.render_frame().unwrap().pixels;
                assert!(wrench.renderer.configure_filtering(policies[index]).is_err());
                assert!(wrench.renderer.render_frame().unwrap().pixels == pixels);
                if mipmaps {
                    let path = root.join(policies[index].name());
                    wrench.api.save_capture(path.clone(), CaptureBits::all());
                    capture_barrier(wrench);
                    let mut replay = Wrench::new_hal(&options, size).unwrap();
                    replay.renderer.configure_filtering(policies[index]).unwrap();
                    let documents = replay.api.load_capture(path.clone(), None);
                    assert_eq!(documents.len(), 1);
                    replay.renderer.prepare_frame(documents[0].document_id).unwrap();
                    assert!(replay.renderer.render_frame().unwrap().pixels == pixels);
                    replay.api.shut_down(true);
                    let mut wrong = Wrench::new_hal(&options, size).unwrap();
                    wrong.renderer.configure_filtering(policies[1 - index]).unwrap();
                    let documents = wrong.api.load_capture(path, None);
                    let error = wrong.renderer.prepare_frame(documents[0].document_id).unwrap_err();
                    assert!(error.contains("Capture filtering"), "{}", error);
                    wrong.api.shut_down(true);
                }
                outputs.push(pixels);
            }
            assert_eq!(outputs[0] != outputs[1], mipmaps);
        }
        for wrench in renderers { wrench.api.shut_down(true); }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn repeated_mipmaps_keep_profiles_isolated() {
        use crate::wrench::Wrench;
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::DeviceIntSize;
        use webrender::hal::Filtering;
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        let mut renderers: Vec<_> = [Filtering::Standard, Filtering::LegacyBrilinear].iter().map(|policy| {
            let mut wrench = Wrench::new_hal(&options, DeviceIntSize::new(320, 224)).unwrap();
            wrench.renderer.configure_filtering(*policy).unwrap();
            wrench
        }).collect();
        for mipmaps in [false, true] {
            let mut outputs = Vec::new();
            for wrench in &mut renderers {
                let mut reader = YamlFrameReader::new(std::path::Path::new("reftests/hal/repeat-mipmaps.yaml"));
                reader.allow_mipmaps(mipmaps);
                reader.build_frame(wrench);
                wrench.renderer.prepare_frame(wrench.document_id).unwrap();
                let frame = wrench.renderer.render_frame().unwrap();
                assert!(frame.pixels.chunks_exact(4).any(|p| p != [255, 255, 255, 255]));
                assert!(wrench.renderer.render_frame().unwrap().pixels == frame.pixels);
                outputs.push(frame.pixels);
                reader.deinit(wrench);
            }
            assert_eq!(outputs[0] != outputs[1], mipmaps);
        }
        for wrench in renderers { wrench.api.shut_down(true); }
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn aa_subdivision_joins_have_coverage() {
        use crate::wrench::Wrench;
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::DeviceIntSize;
        use webrender::hal::Filtering;
        let size = DeviceIntSize::new(1920, 1080);
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        for filtering in [Filtering::Standard, Filtering::LegacyBrilinear] {
            let mut wrench = Wrench::new_hal(&options, size).unwrap();
            wrench.renderer.configure_filtering(filtering).unwrap();
            for (scene, probes) in [
                ("rotated-clip", &[(173usize, 126usize, [0u8, 0, 255, 255])][..]),
                ("perspective-border-radius", &[(283, 182, [0, 0, 255, 255]), (424, 313, [0, 0, 255, 255])][..]),
                ("perspective", &[(966, 308, [255, 118, 118, 255])][..]),
                ("near-plane-clip", &[(168, 70, [191, 64, 64, 255]), (643, 394, [255, 127, 127, 255])][..]),
            ] {
                let path = std::path::Path::new("reftests/transforms").join(format!("{scene}.yaml"));
                let mut reader = YamlFrameReader::new(&path);
                reader.build_frame(&mut wrench);
                wrench.renderer.prepare_frame(wrench.document_id).unwrap();
                let frame = wrench.renderer.render_frame().unwrap();
                for (x, y, expected) in probes {
                    let offset = (y * size.width as usize + x) * 4;
                    let actual = &frame.pixels[offset..offset + 4];
                    assert!(actual.iter().zip(expected).all(|(a, b)| a.abs_diff(*b) <= 1),
                        "{} {:?} ({}, {}): {:?} != {:?}", scene, filtering, x, y, actual, expected);
                }
            }
            wrench.api.shut_down(true);
        }
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn capture_sequence_replay() {
        use crate::wrench::{Wrench, WrenchThing, CapturedSequence};
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::DeviceIntSize;
        use webrender::render_api::CaptureBits;
        let root = std::env::temp_dir().join(format!("wr-hal-sequence-{}", std::process::id()));
        let size = DeviceIntSize::new(800, 600);
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        for bits in [CaptureBits::SCENE | CaptureBits::EXTERNAL_RESOURCES, CaptureBits::all()] {
            let mut original = Wrench::new_hal_with_subpixel(&options, size, false).unwrap();
            original.api.start_capture_sequence(root.clone(), bits);
            capture_barrier(&mut original);
            let mut frames = Vec::new();
            let mut readers = Vec::new();
            for source in ["image/texture-rect.yaml", "image/yuv.yaml", "text/shadow-cover-1.yaml"] {
                let mut reader = YamlFrameReader::new(&std::path::Path::new("reftests").join(source));
                reader.build_frame(&mut original);
                original.renderer.prepare_frame(original.document_id).unwrap();
                frames.push(original.renderer.render_frame().unwrap().pixels);
                capture_barrier(&mut original);
                readers.push(reader);
            }
            original.api.stop_capture_sequence();
            capture_barrier(&mut original);
            original.api.shut_down(true);
            drop(original);
            drop(readers);
            let moved = root.with_extension("moved");
            std::fs::rename(&root, &moved).unwrap();
            let mut replay = Wrench::new_hal_with_subpixel(&options, size, false).unwrap();
            let mut sequence = CapturedSequence::new(moved.clone(), 1, 1);
            for expected in &frames {
                sequence.do_frame(&mut replay);
                replay.renderer.prepare_frame(replay.document_id).unwrap();
                assert!(&replay.renderer.render_frame().unwrap().pixels == expected, "sequence {bits:?}");
                <CapturedSequence as WrenchThing<webrender::hal::Renderer>>::next_frame(&mut sequence);
            }
            <CapturedSequence as WrenchThing<webrender::hal::Renderer>>::prev_frame(&mut sequence);
            sequence.do_frame(&mut replay);
            replay.renderer.prepare_frame(replay.document_id).unwrap();
            assert!(replay.renderer.render_frame().unwrap().pixels == frames[1]);
            replay.api.shut_down(true);
            drop(replay);
            std::fs::remove_dir_all(moved).unwrap();
        }
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn capture_multiple_documents_and_blob() {
        use crate::wrench::Wrench;
        use webrender::api::*;
        use webrender::api::units::*;
        use webrender::render_api::{CaptureBits, Transaction};
        let root = std::env::temp_dir().join(format!("wr-hal-documents-{}", std::process::id()));
        let size = DeviceIntSize::new(64, 64);
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        let mut original = Wrench::new_hal(&options, size).unwrap();
        let ids = [original.document_id, original.api.add_document(size)];
        for (index, id) in ids.iter().copied().enumerate() {
            let pipeline = PipelineId(0, index as u32);
            let blob = original.api.generate_blob_image_key();
            let mut txn = Transaction::new();
            txn.add_blob_image(blob, ImageDescriptor::new(64, 64, ImageFormat::BGRA8, ImageDescriptorFlags::IS_OPAQUE),
                crate::blob::serialize_blob(if index == 0 { ColorU::new(255, 0, 0, 255) } else { ColorU::new(0, 0, 255, 255) }),
                DeviceIntRect::from_size(size), Some(32));
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin(crate::AU_PER_DEV_PX);
            let rect = LayoutRect::from_size(LayoutSize::new(64.0, 64.0));
            let info = CommonItemProperties { clip_rect: rect, clip_chain_id: ClipChainId::INVALID,
                spatial_id: SpatialId::root_scroll_node(pipeline), flags: PrimitiveFlags::default() };
            builder.push_image(&info, rect, ImageRendering::Pixelated, AlphaType::PremultipliedAlpha, blob.as_image(), ColorF::WHITE);
            txn.set_root_pipeline(pipeline);
            txn.set_display_list(Epoch(0), original.api.get_namespace_id(), builder.end());
            txn.generate_frame(0, true, false, RenderReasons::TESTING);
            original.api.send_transaction(id, txn);
        }
        capture_barrier(&mut original);
        let mut expected = std::collections::HashMap::new();
        for id in ids {
            original.renderer.prepare_frame(id).unwrap();
            expected.insert(id, original.renderer.render_frame().unwrap().pixels);
            println!("Captured document {id:?}: {:?}", expected[&id].chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect::<std::collections::BTreeSet<_>>());
        }
        assert!(expected[&ids[0]] != expected[&ids[1]]);
        original.api.save_capture(root.clone(), CaptureBits::all());
        capture_barrier(&mut original);
        original.api.shut_down(true);
        drop(original);
        let mut replay = Wrench::new_hal(&options, size).unwrap();
        let documents = replay.api.load_capture(root.clone(), None);
        assert_eq!(documents.len(), 2);
        capture_barrier(&mut replay);
        for document in documents {
            replay.renderer.prepare_frame(document.document_id).unwrap();
            assert!(replay.renderer.render_frame().unwrap().pixels == expected[&document.document_id]);
            let mut txn = Transaction::new();
            txn.set_root_pipeline(document.root_pipeline_id.unwrap());
            txn.generate_frame(1, true, false, RenderReasons::TESTING);
            replay.api.send_transaction(document.document_id, txn);
            replay.renderer.prepare_frame(document.document_id).unwrap();
            assert!(replay.renderer.render_frame().unwrap().pixels == expected[&document.document_id]);
        }
        replay.api.shut_down(true);
        drop(replay);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn capture_replay_fresh_renderer() {
        use crate::wrench::Wrench;
        use crate::yaml_frame_reader::YamlFrameReader;
        use webrender::api::units::*;
        use webrender::render_api::{CaptureBits, DebugCommand, Transaction};
        let root = std::env::var_os("WR_CAPTURE_TEST_PATH").map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("wr-hal-capture-{}", std::process::id())));
        let capture = root.join("current");
        let moved = root.join("moved");
        std::fs::create_dir_all(&root).unwrap();
        let size = DeviceIntSize::new(800, 600);
        let options = webrender::hal::Options { validation: true, ..Default::default() };
        for mode in 0..3 {
            let config = || match mode {
                1 => crate::hal_compositor::Native::config(DeviceIntPoint::new(13, 7)).0,
                2 => crate::hal_compositor::Layer::config(DeviceIntPoint::new(13, 7)).0,
                _ => webrender::hal::CompositorConfig::Draw,
            };
            for source in ["image/texture-rect.yaml", "image/yuv.yaml", "image/yuv-clip.yaml", "text/shadow-cover-1.yaml"] {
                let mut original = Wrench::new_hal_with_compositor(&options, size, false, None, config()).unwrap();
                let mut reader = YamlFrameReader::new(&std::path::Path::new("reftests").join(source));
                reader.build_frame(&mut original);
                original.renderer.prepare_frame(original.document_id).unwrap();
                let expected = original.renderer.render_frame().unwrap().pixels;
                original.api.save_capture(capture.clone(), CaptureBits::all());
                let (tx, rx) = webrender::api::channel::unbounded_channel();
                original.api.send_debug_cmd(DebugCommand::GetDebugFlags(tx));
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    original.renderer.update().unwrap();
                    if rx.try_recv().is_ok() { break; }
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
                original.api.flush_scene_builder();
                original.api.shut_down(true);
                drop(original);
                drop(reader);
                std::fs::rename(&capture, &moved).unwrap();
                let mut replay = Wrench::new_hal_with_compositor(&options, size, false, None, config()).unwrap();
                let documents = replay.api.load_capture(moved.clone(), None);
                assert_eq!(documents.len(), 1);
                let document = &documents[0];
                replay.document_id = document.document_id;
                replay.renderer.prepare_frame(replay.document_id).unwrap();
                assert!(replay.renderer.render_frame().unwrap().pixels == expected, "built {mode} {source}");
                let mut txn = Transaction::new();
                txn.set_root_pipeline(document.root_pipeline_id.unwrap());
                txn.generate_frame(1, true, false, webrender::api::RenderReasons::TESTING);
                replay.api.send_transaction(replay.document_id, txn);
                replay.renderer.prepare_frame(replay.document_id).unwrap();
                assert!(replay.renderer.render_frame().unwrap().pixels == expected, "rebuilt {mode} {source}");
                replay.api.shut_down(true);
                drop(replay);
                std::fs::remove_dir_all(&moved).unwrap();
            }
        }
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    #[ignore = "Requires Vulkan and validation"]
    fn compositor_adapters_use_native_targets() {
        use crate::wrench::{HeadlessTestWindow, Wrench};
        use webrender::api::units::{DeviceIntPoint, DeviceIntSize};
        for native in [true, false] {
            let (config, trace) = if native {
                crate::hal_compositor::Native::config(DeviceIntPoint::new(13, 7))
            } else {
                crate::hal_compositor::Layer::config(DeviceIntPoint::new(13, 7))
            };
            let (notifier, rx) = crate::create_notifier();
            let size = DeviceIntSize::new(800, 600);
            let mut wrench = Wrench::new_hal_with_compositor(
                &webrender::hal::Options { validation: true, ..Default::default() }, size, true, Some(notifier), config,
            ).unwrap();
            let mut window = HeadlessTestWindow(size);
            crate::rawtest::RawtestHarness::new(&mut wrench, &mut window, &rx)
                .run_selected(Some("test_resize_image")).unwrap();
            wrench.renderer.poll().unwrap();
            {
                let trace = trace.borrow();
                assert!(trace.commits >= 3);
                assert!(trace.releases > 0);
                assert_eq!(trace.abandoned, 0);
                if native {
                    assert!(trace.updates > 0);
                    assert!(!trace.binds.is_empty());
                } else {
                    assert!(trace.layers >= 3);
                }
            }
            wrench.api.shut_down(true);
        }
    }

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
        use webrender::render_api::{Transaction, DebugCommand, ClearCache, CaptureBits};
        use std::{rc::Rc, cell::RefCell};
        use webrender::hal::{Filtering, ExternalImageDevice, ExternalImageLease, ExternalImageProvider, ExternalImageSource, ExternalImageRelease, NativeImage, ReadbackHandle};
        struct Provider { current: Rc<RefCell<Option<NativeImage>>>, releases: Rc<RefCell<Vec<ExternalImageRelease>>> }
        impl ExternalImageProvider for Provider {
            fn acquire(&mut self, _: ExternalImageId, _: u8, _: bool) -> Result<ExternalImageLease, String> {
                let image = self.current.borrow().as_ref().unwrap().clone();
                let descriptor = image.descriptor();
                let releases = self.releases.clone();
                ExternalImageLease::new(descriptor, TexelRect::new(0.0, 0.0, descriptor.size.width as f32, descriptor.size.height as f32),
                    image.generation(), ExternalImageSource::Native(image), move |status| releases.borrow_mut().push(status))
            }
        }
        struct State {
            wrench: Wrench<webrender::hal::Renderer>,
            image: ImageKey,
            mip_image: ImageKey,
            mip_color: [u8; 4],
            font: FontInstanceKey,
            glyphs: Vec<u32>,
            color: [u8; 4],
            previous: Option<Vec<u8>>,
            producer: ExternalImageDevice,
            native: Rc<RefCell<Option<NativeImage>>>,
            releases: Rc<RefCell<Vec<ExternalImageRelease>>>,
            pending: Vec<(ReadbackHandle, DeviceIntSize, [u8; 4], [u8; 4])>,
        }
        let mut states = Vec::new();
        for policy in [Filtering::Standard, Filtering::LegacyBrilinear] {
            let mut wrench = Wrench::new_hal(
                &webrender::hal::Options {
                    validation: true,
                    ..Default::default()
                },
                DeviceIntSize::new(257, 129),
            )
            .unwrap();
            wrench.renderer.configure_filtering(policy).unwrap();
            println!("HAL_STRESS_PROFILE {:?}", policy);
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
            let mip_image = wrench.api.generate_image_key();
            transaction.add_image(mip_image,
                ImageDescriptor::new(640, 640, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE | ImageDescriptorFlags::ALLOW_MIPMAPS),
                ImageData::new([255, 255, 0, 255].repeat(640*640)), None);
            wrench.api.send_transaction(wrench.document_id, transaction);
            let image = wrench.api.generate_image_key();
            let producer = wrench.renderer.external_image_device();
            let native = Rc::new(RefCell::new(None));
            let releases = Rc::new(RefCell::new(Vec::new()));
            wrench.renderer.set_external_image_provider(Box::new(Provider { current: native.clone(), releases: releases.clone() })).unwrap();
            states.push(State {
                wrench,
                image,
                mip_image,
                mip_color: [255, 255, 0, 255],
                font,
                glyphs,
                color: [255, 0, 0, 255],
                previous: None, producer, native, releases, pending: Vec::new(),
            });
        }
        let mut observed_pending = false;
        let mut samples = Vec::new();
        for index in 0..1100 {
            let which = index % 2;
            let serial = index / 2;
            let state = &mut states[which];
            let producer = &state.producer;
            let wrench = &mut state.wrench;
            wrench.renderer.poll().unwrap();
            let mut pending = 0;
            while pending < state.pending.len() {
                let (handle, extent, color, mip_color) = state.pending[pending];
                if let Some(pixels) = wrench.renderer.poll_readback(handle).unwrap() {
                    let offset = ((extent.height as usize - 1 - 20) * extent.width as usize + 20) * 4;
                    assert_eq!(&pixels[offset..offset + 4], &color);
                    let offset = ((extent.height as usize - 1 - 12) * extent.width as usize + 248) * 4;
                    assert_eq!(&pixels[offset..offset + 4], &mip_color);
                    state.pending.remove(pending);
                } else { pending += 1; }
            }
            assert!(state.pending.len() < 8);
            let size = DeviceIntSize::new(257 + if serial / 40 % 2 == 0 { 0 } else { 7 }, 129);
            let replace = serial % 100 == 0;
            let rebuild = serial % 5 == 0 || replace;
            let update = serial % 4 != 0 || replace;
            let present = serial % 31 != 0;
            let mut transaction = Transaction::new();
            if serial % 40 == 0 {
                state.mip_color = if serial / 40 % 2 == 0 { [255, 255, 0, 255] } else { [0, 255, 255, 255] };
                transaction.update_image(state.mip_image,
                    ImageDescriptor::new(640, 640, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE | ImageDescriptorFlags::ALLOW_MIPMAPS),
                    ImageData::new(state.mip_color.repeat(640*640)), &DirtyRect::All);
            }
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
                let bytes = state.color.repeat(width as usize * 7);
                let data = if which == 0 {
                    ImageData::new(bytes)
                } else {
                    let update = state.native.borrow().as_ref().map_or(false, |image|
                        image.descriptor().size == descriptor.size && producer.update_image(image, descriptor, &bytes).is_ok());
                    if !update { *state.native.borrow_mut() = Some(state.producer.create_image(descriptor, &bytes).unwrap()); }
                    ImageData::External(ExternalImageData { id: ExternalImageId(22), channel_index: 0,
                        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D), normalized_uvs: false })
                };
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
                builder.push_image(&common, rect(240.0, 4.0, 16.0, 16.0), ImageRendering::Auto,
                    AlphaType::PremultipliedAlpha, state.mip_image, ColorF::WHITE);
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
                assert_eq!(pixel(248, 12), state.mip_color);
                assert_eq!(pixel(250, 120), [255, 255, 255, 255]);
                if !update && !rebuild {
                    if let Some(previous) = &state.previous {
                        assert_eq!(previous, &frame.pixels);
                    }
                }
                state.previous = Some(frame.pixels);
                let handle = wrench.renderer.request_readback(FramebufferIntRect::from_size(FramebufferIntSize::new(size.width, size.height))).unwrap();
                state.pending.push((handle, size, state.color, state.mip_color));
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
        let capture_root = std::env::temp_dir().join(format!("wr-hal-mixed-stress-{}", std::process::id()));
        for which in 0..2 {
            let values: Vec<_> = samples
                .iter()
                .filter(|(id, _)| *id == which)
                .map(|(_, bytes)| *bytes)
                .collect();
            assert!(values.iter().copied().max().unwrap() < 256 * 1024 * 1024);
            assert!(values.last().unwrap() <= &(values[0] + 32 * 1024 * 1024));
            let state = &mut states[which];
            for (handle, extent, color, mip_color) in state.pending.drain(..) {
                let pixels = state.wrench.renderer.wait_readback(handle).unwrap();
                let offset = ((extent.height as usize - 1 - 20) * extent.width as usize + 20) * 4;
                assert_eq!(&pixels[offset..offset + 4], &color);
                let offset = ((extent.height as usize - 1 - 12) * extent.width as usize + 248) * 4;
                assert_eq!(&pixels[offset..offset + 4], &mip_color);
            }
            state.wrench.renderer.poll().unwrap();
            assert!(!state.releases.borrow().contains(&ExternalImageRelease::Abandoned));
            if which == 1 { assert!(state.releases.borrow().len() > 100); }
            let policy = state.wrench.renderer.filtering();
            let capture = capture_root.join(policy.name());
            state.wrench.api.save_capture(capture.clone(), CaptureBits::all());
            capture_barrier(&mut state.wrench);
            let expected = state.previous.as_ref().unwrap();
            let size = DeviceIntSize::new((expected.len() / (129*4)) as i32, 129);
            let mut replay = Wrench::new_hal(&webrender::hal::Options { validation: true, ..Default::default() }, size).unwrap();
            replay.renderer.configure_filtering(policy).unwrap();
            let documents = replay.api.load_capture(capture, None);
            assert_eq!(documents.len(), 1);
            replay.renderer.prepare_frame(documents[0].document_id).unwrap();
            assert!(&replay.renderer.render_frame().unwrap().pixels == expected);
            replay.api.shut_down(true);
            state.wrench.api.shut_down(true);
        }
        std::fs::remove_dir_all(capture_root).unwrap();
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
pub(crate) fn filtering(args: &clap::ArgMatches) -> webrender::hal::Filtering {
    match args.value_of("hal_filtering").unwrap_or("standard") {
        "standard" => webrender::hal::Filtering::Standard,
        "legacy-brilinear" => webrender::hal::Filtering::LegacyBrilinear,
        _ => unreachable!(),
    }
}

#[cfg(feature = "hal-vulkan")]
pub(crate) fn compositor_clips_override(args: &clap::ArgMatches) -> Result<Option<bool>, String> {
    match args.value_of("compositor_clips") {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(value) => Err(format!("Unexpected --compositor-clips value {value}")),
    }
}

#[cfg(feature = "hal-vulkan")]
pub(crate) fn compositor_config(args: &clap::ArgMatches) -> Result<webrender::hal::CompositorConfig, String> {
    match args.value_of("hal_compositor").unwrap_or("draw") {
        "draw" => Ok(webrender::hal::CompositorConfig::Draw),
        "native" => Ok(crate::hal_compositor::Native::config(webrender::api::units::DeviceIntPoint::new(3, 5)).0),
        "layer" => Ok(crate::hal_compositor::Layer::config(webrender::api::units::DeviceIntPoint::new(3, 5)).0),
        mode => Err(format!("Unsupported HAL compositor {mode}")),
    }
}

#[cfg(feature = "hal-vulkan")]
fn run(args: &clap::ArgMatches) -> Result<(), String> {
    use webrender::hal::{create_vulkan_device, Options, Readback};

    if args.value_of("hal_filtering").is_some()
        && !matches!(args.subcommand_name(), Some("png" | "show" | "reftest" | "rawtest" | "test_invalidation")) {
        return Err("--hal-filtering requires a renderer command".into());
    }

    if args.subcommand_name() == Some("test_surface") {
        if args.is_present("headless") || args.is_present("software") || args.is_present("angle") {
            return Err("test_surface requires a native Vulkan window".into());
        }
        return crate::hal_surface::run(Options {
            adapter_name: args.value_of("hal_adapter").map(str::to_owned),
            validation: args.is_present("hal_validation"),
        });
    }
    if !args.is_present("headless") && args.subcommand_name() != Some("show") {
        return Err("This HAL command requires --headless".into());
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
            return Err(format!("--{option} is not supported by the HAL command path"));
        }
    }
    let command = args.subcommand_name().unwrap_or("");
    if !matches!(command, "test_init" | "test_hal" | "png" | "reftest" | "rawtest" | "test_invalidation" | "show") {
        return Err(format!(
            "{command:?} is not implemented for HAL yet"
        ));
    }
    let dimensions = match args.value_of("size") {
        None if matches!(command, "png" | "reftest" | "rawtest" | "test_invalidation" | "show") => [1920, 1080],
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
    if command == "show" {
        if !args.is_present("headless") { return crate::hal_window::run(args, &options, dimensions); }
        if args.value_of("hal_windows").is_some() { return Err("--hal-windows requires windowed show".into()); }
        use crate::wrench::Wrench;
        let size = webrender::api::units::DeviceIntSize::new(dimensions[0] as i32, dimensions[1] as i32);
        let mut wrench = Wrench::new_hal_with_compositor(&options, size, !args.is_present("no_subpixel_aa"), None, compositor_config(args)?)?;
        wrench.renderer.configure_filtering(filtering(args))?;
        if let Some(enabled) = compositor_clips_override(args)? { wrench.set_compositor_clips_override(enabled); }
        println!("Backend: wgpu-hal/Vulkan; adapter: {}", wrench.renderer.info().name);
        let show = args.subcommand_matches("show").unwrap();
        let path = std::path::Path::new(show.value_of("INPUT").unwrap());
        let mut thing = playback(&mut wrench, path, Some(show))?;
        let count: usize = args.value_of("hal_frames").unwrap_or("1").parse().map_err(|_| "Invalid HAL frame count")?;
        if count == 0 { return Err("HAL frame count must be positive".into()); }
        for _ in 0..count {
            thing.do_frame(&mut wrench);
            wrench.renderer.prepare_frame(wrench.document_id)?;
            wrench.renderer.render()?;
            wrench.renderer.poll()?;
            thing.next_frame();
        }
        wrench.api.shut_down(true);
        return Ok(());
    }
    if matches!(command, "rawtest" | "test_invalidation") {
        use crate::wrench::{HeadlessTestWindow, Wrench};
        let (notifier, rx) = crate::create_notifier();
        let size = webrender::api::units::DeviceIntSize::new(dimensions[0] as i32, dimensions[1] as i32);
        let mut wrench = Wrench::new_hal_with_compositor(&options, size, !args.is_present("no_subpixel_aa"), Some(notifier), compositor_config(args)?)?;
        wrench.renderer.configure_filtering(filtering(args))?;
        if let Some(enabled) = compositor_clips_override(args)? { wrench.set_compositor_clips_override(enabled); }
        println!("Backend: wgpu-hal/Vulkan; adapter: {}", wrench.renderer.info().name);
        let mut window = HeadlessTestWindow(size);
        let result = if command == "rawtest" {
            let filter = args.subcommand_matches("rawtest").unwrap().value_of("TEST");
            crate::rawtest::RawtestHarness::new(&mut wrench, &mut window, &rx).run_selected(filter)
        } else {
            let failures = crate::test_invalidation::TestHarness::new(&mut wrench, &mut window, &rx).run();
            if failures == 0 { Ok(()) } else { Err(format!("{failures} HAL invalidation tests failed")) }
        };
        wrench.api.shut_down(true);
        return result;
    }
    if command == "reftest" {
        use crate::reftest::{ReftestHarness, ReftestOptions};
        use crate::wrench::Wrench;
        use std::path::Path;
        let size =
            webrender::api::units::DeviceIntSize::new(dimensions[0] as i32, dimensions[1] as i32);
        let mut wrench =
            Wrench::new_hal_with_compositor(&options, size, !args.is_present("no_subpixel_aa"), None, compositor_config(args)?)?;
        wrench.renderer.configure_filtering(filtering(args))?;
        if let Some(enabled) = compositor_clips_override(args)? { wrench.set_compositor_clips_override(enabled); }
        println!("HAL filtering: {}", wrench.renderer.filtering().name());
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
    use webrender::api::units::DeviceIntSize;
    use std::convert::TryFrom;
    let enable_subpixel_aa = !args.is_present("no_subpixel_aa");
    let compositor = compositor_config(args)?;
    let filtering = filtering(args);
    let compositor_clips = compositor_clips_override(args)?;
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
        Wrench::new_hal_with_compositor(options, size, enable_subpixel_aa, None, compositor)?;
    wrench.renderer.configure_filtering(filtering)?;
    if let Some(enabled) = compositor_clips { wrench.set_compositor_clips_override(enabled); }
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
    let mut reader = playback(&mut wrench, &input, None)?;
    reader.do_frame(&mut wrench);
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

#[cfg(feature = "hal-vulkan")]
pub(crate) fn playback(wrench: &mut crate::wrench::Wrench<webrender::hal::Renderer>, path: &std::path::Path,
            args: Option<&clap::ArgMatches>) -> Result<Box<dyn crate::wrench::WrenchThing<webrender::hal::Renderer>>, String> {
    if path.join("scenes").is_dir() {
        let sequence_id = |name| args.and_then(|args| args.value_of(name)).unwrap_or("1")
            .parse::<u32>().map_err(|_| format!("Invalid {name}"));
        Ok(Box::new(crate::wrench::CapturedSequence::new(path.to_owned(), sequence_id("scene-id")?, sequence_id("frame-id")?)))
    } else if path.is_dir() {
        let mut documents = wrench.api.load_capture(path.to_owned(), None);
        if documents.is_empty() { return Err("Capture contains no documents".into()); }
        let captured = documents.remove(0);
        wrench.document_id = captured.document_id;
        Ok(Box::new(captured))
    } else {
        Ok(Box::new(match args {
            Some(args) => crate::yaml_frame_reader::YamlFrameReader::new_from_show_args(args),
            None => crate::yaml_frame_reader::YamlFrameReader::new(path),
        }))
    }
}
