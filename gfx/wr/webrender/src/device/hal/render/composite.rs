/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::composite::{CompositeState, CompositeTile, TileKind};
use crate::rectangle_occlusion::FrontToBackBuilder;
use crate::segment::SegmentBuilder;

#[derive(Clone, Copy)]
struct RegionKey {
    tile_index: usize,
    needs_mask: bool,
}

pub(super) struct Region {
    pub tile_index: usize,
    pub rect: DeviceRect,
    pub needs_mask: bool,
    pub blend: u8,
}

fn add_region(
    occlusion: &mut FrontToBackBuilder<RegionKey>,
    segments: &mut SegmentBuilder,
    tile_index: usize,
    rect: DeviceRect,
    opaque: bool,
    clip: Option<&CompositorClip>,
) {
    if let Some(clip) = clip {
        segments.initialize(rect.cast_unit(), None);
        segments.push_clip_rect(clip.rect.cast_unit(), Some(clip.radius), None, api::ClipMode::Clip);
        segments.build(|segment| {
            occlusion.add(&segment.rect.cast_unit(), opaque && !segment.has_mask,
                RegionKey { tile_index, needs_mask: segment.has_mask });
        });
    } else {
        occlusion.add(&rect, opaque, RegionKey { tile_index, needs_mask: false });
    }
}

fn visible_regions(occlusion: &FrontToBackBuilder<RegionKey>) -> Vec<Region> {
    occlusion.opaque_items().iter().map(|item| (item, 0))
        .chain(occlusion.alpha_items().iter().rev().map(|item| (item, 1)))
        .map(|(item, blend)| Region {
            tile_index: item.key.tile_index,
            rect: item.rectangle,
            needs_mask: item.key.needs_mask,
            blend,
        }).collect()
}

pub(super) fn regions(
    state: &CompositeState,
    frame_rect: DeviceIntRect,
    damage: DeviceIntRect,
    optimize: bool,
) -> Vec<Region> {
    let clipped = |tile: &CompositeTile| {
        let valid = state.get_device_rect(&tile.local_valid_rect, tile.transform_index);
        tile.device_clip_rect.intersection(&valid)
            .and_then(|rect| rect.intersection(&frame_rect.to_f32()))
            .filter(|rect| rect.round_out().to_i32().intersects(&damage))
    };
    if !optimize {
        return state.tiles.iter().enumerate().rev().filter_map(|(tile_index, tile)| {
            clipped(tile).map(|rect| Region {
                tile_index, rect, needs_mask: tile.clip_index.is_some(), blend: 1,
            })
        }).collect();
    }
    let mut occlusion = FrontToBackBuilder::with_capacity(state.tiles.len(), state.tiles.len());
    let mut segments = SegmentBuilder::new();
    for (tile_index, tile) in state.tiles.iter().enumerate() {
        let Some(rect) = clipped(tile) else { continue; };
        let tile_rect = state.get_device_rect(&tile.local_rect, tile.transform_index);
        let Some(rect) = rect.intersection(&tile_rect)
            .and_then(|rect| rect.intersection(&damage.to_f32())) else { continue; };
        add_region(&mut occlusion, &mut segments, tile_index, rect, tile.kind == TileKind::Opaque,
            tile.clip_index.map(|index| state.get_compositor_clip(index)));
    }
    visible_regions(&occlusion)
}

pub(super) fn push_draw<'a, A: hal::Api>(draws: &mut Vec<Draw<'a, A>>, draw: Draw<'a, A>) {
    if let Some(previous) = draws.last_mut() {
        let compatible = previous.shader == draw.shader && previous.blend == draw.blend
            && previous.depth == draw.depth && previous.filter == draw.filter
            && previous.scissor == draw.scissor && previous.count_in_stats == draw.count_in_stats
            && previous.clear_color.is_none() && draw.clear_color.is_none()
            && previous.readback.is_none() && draw.readback.is_none()
            && previous.textures.colors.iter().zip(&draw.textures.colors)
                .all(|(a, b)| Rc::ptr_eq(a, b))
            && Rc::ptr_eq(&previous.textures.clip, &draw.textures.clip);
        if compatible {
            if let (Some(count), Instances::Owned(instances)) =
                (previous.count.checked_add(draw.count), &mut previous.instances) {
                instances.extend_from_slice(draw.instances.bytes());
                previous.count = count;
                return;
            }
        }
    }
    draws.push(draw);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> DeviceRect {
        DeviceRect::from_origin_and_size(DevicePoint::new(x, y), DeviceSize::new(width, height))
    }

    #[test]
    fn opaque_regions_remove_covered_tiles_and_preserve_alpha_order() {
        let mut occlusion = FrontToBackBuilder::with_capacity(4, 4);
        let mut segments = SegmentBuilder::new();
        let front = rect(2.0, 2.0, 6.0, 6.0);
        add_region(&mut occlusion, &mut segments, 0, front, false, None);
        add_region(&mut occlusion, &mut segments, 1, front, true, None);
        add_region(&mut occlusion, &mut segments, 2, front, true, None);
        add_region(&mut occlusion, &mut segments, 3, rect(0.0, 0.0, 10.0, 10.0), false, None);
        let regions = visible_regions(&occlusion);
        assert_eq!(regions[0].tile_index, 1);
        assert_eq!(regions[0].blend, 0);
        assert!(!regions.iter().any(|region| region.tile_index == 2));
        assert_eq!(regions.last().unwrap().tile_index, 0);
        let background: Vec<_> = regions.iter().filter(|region| region.tile_index == 3).collect();
        assert!(background.iter().all(|region| !region.rect.intersects(&front) && region.blend == 1));
        assert_eq!(background.iter().map(|region| region.rect.area()).sum::<f32>(), 64.0);
    }

    #[test]
    fn rounded_opaque_tiles_preserve_background_under_masked_corners() {
        let mut occlusion = FrontToBackBuilder::with_capacity(4, 4);
        let mut segments = SegmentBuilder::new();
        let full = rect(0.0, 0.0, 20.0, 20.0);
        let clip = CompositorClip { rect: full, radius: api::BorderRadius::uniform(5.0) };
        add_region(&mut occlusion, &mut segments, 0, full, true, Some(&clip));
        add_region(&mut occlusion, &mut segments, 1, full, true, None);
        let regions = visible_regions(&occlusion);
        let corners: Vec<_> = regions.iter().filter(|region| region.tile_index == 0 && region.needs_mask).collect();
        assert!(!corners.is_empty());
        assert!(corners.iter().all(|region| region.blend == 1));
        assert!(regions.iter().any(|region| region.tile_index == 0 && region.blend == 0 && !region.needs_mask));
        for corner in corners {
            assert!(regions.iter().any(|region| region.tile_index == 1 && region.rect.intersects(&corner.rect)));
        }
        let opaque_area: f32 = regions.iter().filter(|region| region.blend == 0).map(|region| region.rect.area()).sum();
        assert_eq!(opaque_area, full.area());
    }

    #[test]
    fn mirrored_opaque_tile_bounds_limit_occlusion_inside_partial_damage() {
        use crate::internal_types::FrameMemory;
        use crate::util::ScaleOffset;
        let memory = FrameMemory::fallback();
        let mut state = CompositeState::new(Default::default(), 1024, true, false, &memory);
        let mirrored = state.register_transform(ScaleOffset::identity(), ScaleOffset::new(-1.0, 1.0, 20.0, 0.0));
        let identity = state.register_transform(ScaleOffset::identity(), ScaleOffset::identity());
        let full = rect(0.0, 0.0, 30.0, 10.0);
        for (local_rect, valid, transform_index) in [
            (rect(0.0, 0.0, 10.0, 10.0), rect(-20.0, 0.0, 40.0, 10.0), mirrored),
            (full, full, identity),
        ] {
            state.tiles.push(CompositeTile {
                surface: CompositeTileSurface::Color { color: ColorF::WHITE },
                local_rect: local_rect.cast_unit(), local_valid_rect: valid.cast_unit(),
                local_dirty_rect: local_rect.cast_unit(), device_clip_rect: full,
                z_id: crate::gpu_types::ZBufferId::invalid(), kind: TileKind::Opaque,
                transform_index, clip_index: None, tile_id: None,
            });
        }
        let damage = rect(8.0, 2.0, 10.0, 5.0).to_i32();
        let visible = regions(&state, full.to_i32(), damage, true);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].tile_index, 0);
        assert_eq!(visible[0].rect, rect(10.0, 2.0, 8.0, 5.0));
        assert_eq!(visible[1].tile_index, 1);
        assert_eq!(visible[1].rect, rect(8.0, 2.0, 2.0, 5.0));
        assert!(visible.iter().all(|region| region.blend == 0 && !region.needs_mask));
        let legacy = regions(&state, full.to_i32(), damage, false);
        assert_eq!(legacy.len(), 2);
        assert_eq!(legacy[0].tile_index, 1);
        assert_eq!(legacy[1].tile_index, 0);
        assert!(legacy.iter().all(|region| region.blend == 1 && region.rect == full));
    }

    #[cfg(wr_hal_vulkan)]
    #[test]
    #[ignore = "Requires Vulkan"]
    fn adjacent_composite_batches_preserve_instances_and_texture_identity() {
        let owner = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let renderer = FrameRenderer::new(owner).unwrap();
        let texture = Texture::new(&renderer.owner, 1, 1, wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Linear, true).unwrap();
        let view = texture.mip_view(0).unwrap();
        let make_draw = |texture: Rc<Texture<hal::api::Vulkan>>, blend, marker| Draw {
            shader: Shader::Composite, blend, depth: 0, count: 1,
            instances: Instances::Owned(vec![marker; CompositeInstance::SIZE]),
            textures: renderer.single_texture(texture), filter: None, clear_color: None,
            count_in_stats: true, readback: None, scissor: rect(0.0, 0.0, 1.0, 1.0).to_i32(),
        };
        let mut draws = Vec::new();
        push_draw(&mut draws, make_draw(texture.clone(), 1, 1));
        push_draw(&mut draws, make_draw(texture.clone(), 1, 2));
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].count, 2);
        assert_eq!(draws[0].instances.bytes(), [vec![1; CompositeInstance::SIZE], vec![2; CompositeInstance::SIZE]].concat());
        push_draw(&mut draws, make_draw(view, 1, 3));
        push_draw(&mut draws, make_draw(texture.clone(), 1, 4));
        push_draw(&mut draws, make_draw(texture, 0, 5));
        assert_eq!(draws.len(), 4);
        assert_eq!(draws.iter().map(|draw| draw.count).collect::<Vec<_>>(), [2, 1, 1, 1]);
    }
}
