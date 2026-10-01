/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn capabilities() -> hal::SurfaceCapabilities {
    hal::SurfaceCapabilities {
        formats: vec![wgt::SurfaceFormatCapabilities {
            format: wgt::TextureFormat::Bgra8Unorm,
            color_spaces: wgt::SurfaceColorSpaces::SRGB,
        }],
        maximum_frame_latency: 1..=3,
        current_extent: None,
        usage: wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::COPY_DST,
        present_modes: vec![wgt::PresentMode::Fifo],
        composite_alpha_modes: vec![
            wgt::CompositeAlphaMode::Opaque,
            wgt::CompositeAlphaMode::PreMultiplied,
        ],
    }
}

fn config(caps: &hal::SurfaceCapabilities, options: SurfaceOptions) -> hal::SurfaceConfiguration {
    negotiate(caps, [137, 99], options, 4096).unwrap()
}

#[test]
fn direct_rendering_only_requests_color_attachment_usage() {
    let mut caps = capabilities();
    for format in [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ] {
        caps.formats[0].format = format;
        for copy in [false, true] {
            caps.usage.set(wgt::TextureUses::COPY_DST, copy);
            let chosen = config(&caps, SurfaceOptions::default());
            assert_eq!(chosen.format, format);
            assert_eq!(chosen.color_space, wgt::SurfaceColorSpace::Srgb);
            assert_eq!(chosen.usage, wgt::TextureUses::COLOR_TARGET);
            assert!(chosen.view_formats.is_empty());
        }
    }
    caps.usage = wgt::TextureUses::COPY_DST;
    assert!(negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).is_err());
    caps.usage = wgt::TextureUses::empty();
    assert!(negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).is_err());
}

#[test]
fn direct_rendering_rejects_formats_that_change_color_encoding() {
    let mut caps = capabilities();
    for format in [
        wgt::TextureFormat::Rgba8UnormSrgb,
        wgt::TextureFormat::Bgra8UnormSrgb,
        wgt::TextureFormat::Rgba16Float,
    ] {
        caps.formats[0].format = format;
        assert!(negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).is_err());
    }
    caps = capabilities();
    caps.formats[0].color_spaces = wgt::SurfaceColorSpaces::empty();
    assert!(negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).is_err());
}

#[test]
fn supported_unorm_format_takes_precedence_over_srgb_attachment() {
    let mut caps = capabilities();
    caps.formats.insert(
        0,
        wgt::SurfaceFormatCapabilities {
            format: wgt::TextureFormat::Rgba8UnormSrgb,
            color_spaces: wgt::SurfaceColorSpaces::SRGB,
        },
    );
    assert_eq!(
        config(&caps, SurfaceOptions::default()).format,
        wgt::TextureFormat::Bgra8Unorm
    );
}

#[test]
fn present_mode_respects_vsync_and_falls_back_to_fifo() {
    let mut caps = capabilities();
    let no_vsync = SurfaceOptions {
        vsync: false,
        ..Default::default()
    };
    assert_eq!(config(&caps, no_vsync).present_mode, wgt::PresentMode::Fifo);
    caps.present_modes.push(wgt::PresentMode::Immediate);
    assert_eq!(
        config(&caps, no_vsync).present_mode,
        wgt::PresentMode::Immediate
    );
    assert_eq!(
        config(&caps, SurfaceOptions::default()).present_mode,
        wgt::PresentMode::Fifo
    );
    caps.present_modes = vec![wgt::PresentMode::Immediate];
    assert!(negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).is_err());
}

#[test]
fn transparent_output_requires_compatible_alpha_composition() {
    let mut caps = capabilities();
    let transparent = SurfaceOptions {
        transparent: true,
        ..Default::default()
    };
    assert_eq!(
        config(&caps, transparent).composite_alpha_mode,
        wgt::CompositeAlphaMode::PreMultiplied
    );
    caps.composite_alpha_modes = vec![wgt::CompositeAlphaMode::Inherit];
    assert_eq!(
        config(&caps, transparent).composite_alpha_mode,
        wgt::CompositeAlphaMode::Inherit
    );
    assert!(negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).is_err());
    for mode in [
        wgt::CompositeAlphaMode::Opaque,
        wgt::CompositeAlphaMode::PostMultiplied,
    ] {
        caps.composite_alpha_modes = vec![mode];
        assert!(negotiate(&caps, [137, 99], transparent, 4096).is_err());
    }
}

#[test]
fn surface_extent_and_frame_latency_follow_driver_limits() {
    let mut caps = capabilities();
    let options = SurfaceOptions::default();
    let chosen = config(&caps, options);
    assert_eq!((chosen.extent.width, chosen.extent.height), (137, 99));
    caps.current_extent = Some(wgt::Extent3d {
        width: 71,
        height: 59,
        depth_or_array_layers: 1,
    });
    let chosen = config(&caps, options);
    assert_eq!((chosen.extent.width, chosen.extent.height), (71, 59));
    for (range, expected) in [(1..=1, 1), (1..=3, 2), (3..=4, 3)] {
        caps.maximum_frame_latency = range;
        assert_eq!(config(&caps, options).maximum_frame_latency, expected);
    }
    for size in [[0, 99], [137, 0], [4097, 99], [137, 4097]] {
        caps.current_extent = None;
        assert!(negotiate(&caps, size, options, 4096).is_err());
        caps.current_extent = Some(wgt::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        });
        assert!(negotiate(&caps, [137, 99], options, 4096).is_err());
    }
}
