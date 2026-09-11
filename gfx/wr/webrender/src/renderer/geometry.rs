/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::units::*;
use crate::render_task::{RenderTask, RenderTaskKind, ReadbackTask};
use crate::util::ScaleOffset;

pub(crate) fn resolve_rects(
    source: &RenderTask,
    destination: &RenderTask,
    transform: &ScaleOffset,
) -> Option<(DeviceIntRect, DeviceIntRect)> {
    let source_info = match &source.kind {
        RenderTaskKind::Picture(info) => info,
        _ => panic!("Resolve source is not a picture"),
    };
    let destination_info = match &destination.kind {
        RenderTaskKind::Picture(info) => info,
        _ => panic!("Resolve destination is not a picture"),
    };
    let source_rect = source.get_target_rect().to_f32();
    let destination_rect = DeviceRect::from_origin_and_size(
        destination.get_target_rect().min.to_f32(),
        destination_info.content_size.to_f32(),
    );
    // Expanded blur targets must use content size, in the destination's raster space.
    let wanted_destination: WorldRect =
        DeviceRect::from_origin_and_size(destination_info.content_origin, destination_rect.size())
            .cast_unit()
            * destination_info.device_pixel_scale.inverse();
    let wanted = transform.map_rect(&wanted_destination);
    let available: WorldRect =
        DeviceRect::from_origin_and_size(source_info.content_origin, source_rect.size())
            .cast_unit()
            * source_info.device_pixel_scale.inverse();
    let intersection = wanted.intersection(&available)?;
    let source_intersection: DeviceRect =
        (intersection * source_info.device_pixel_scale).cast_unit();
    let destination_intersection: DeviceRect =
        (transform.unmap_rect(&intersection) * destination_info.device_pixel_scale).cast_unit();
    let source_origin = source_rect.min + source_intersection.min.to_vector()
        - source_info.content_origin.to_vector();
    let destination_origin = destination_rect.min + destination_intersection.min.to_vector()
        - destination_info.content_origin.to_vector();
    Some((
        DeviceIntRect::from_origin_and_size(
            source_origin.to_i32(),
            source_intersection.size().round().to_i32(),
        ),
        DeviceIntRect::from_origin_and_size(
            destination_origin.to_i32(),
            destination_intersection.size().round().to_i32(),
        ),
    ))
}

pub(crate) fn readback_rects(
    backdrop: &RenderTask,
    readback: &RenderTask,
) -> Option<(DeviceIntRect, DeviceIntRect)> {
    let origin = match readback.kind {
        RenderTaskKind::Readback(ReadbackTask {
            readback_origin, ..
        }) => readback_origin?,
        _ => panic!("Readback task has an unexpected kind"),
    };
    let backdrop_origin = match &backdrop.kind {
        RenderTaskKind::Picture(info) => info.content_origin,
        _ => panic!("Readback source is not a picture"),
    };
    let readback_rect = readback.get_target_rect();
    let backdrop_rect = backdrop.get_target_rect();
    let wanted = DeviceRect::from_origin_and_size(origin, readback_rect.size().to_f32());
    let available =
        DeviceRect::from_origin_and_size(backdrop_origin, backdrop_rect.size().to_f32());
    let intersection = wanted.intersection(&available)?;
    let source_origin =
        backdrop_rect.min.to_f32() + intersection.min.to_vector() - backdrop_origin.to_vector();
    let destination_origin =
        readback_rect.min.to_f32() + intersection.min.to_vector() - origin.to_vector();
    let size = intersection.size().to_i32();
    Some((
        DeviceIntRect::from_origin_and_size(source_origin.to_i32(), size),
        DeviceIntRect::from_origin_and_size(destination_origin.to_i32(), size),
    ))
}
