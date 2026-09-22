/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub(super) struct Table<A: BackendApi> {
    bytes: Vec<u8>,
    buffer: Option<Rc<Buffer<A>>>,
    texture_current: bool,
}

impl<A: BackendApi> Table<A> {
    pub(super) fn release_buffer(&mut self) {
        self.buffer = None;
    }
}

impl<A: BackendApi> HalGpuBackend<A> {
    pub(super) fn table_pixels(
        &self,
        texture: &Texture<A>,
        rect: DeviceIntRect,
    ) -> Option<Vec<u8>> {
        let table = self.tables.get(&texture.allocation_id)?;
        let pitch = texture.size.width as usize * 16;
        let mut output = Vec::with_capacity(rect.area() as usize * 16);
        for y in rect.min.y..rect.max.y {
            let start = y as usize * pitch + rect.min.x as usize * 16;
            output.extend_from_slice(&table.bytes[start..start + rect.width() as usize * 16]);
        }
        Some(output)
    }

    pub(super) fn upload_table(
        &mut self,
        texture: &wr::Texture,
        rect: DeviceIntRect,
        stride: Option<i32>,
        data: &[u8],
    ) -> bool {
        if !matches!(texture.format, ImageFormat::RGBAF32 | ImageFormat::RGBAI32)
            || texture.size.width as usize != MAX_VERTEX_TEXTURE_WIDTH
        {
            return false;
        }
        let raw = &self.textures[&texture.id];
        let pitch = texture.size.width as usize * 16;
        let table = self
            .tables
            .entry(raw.allocation_id)
            .or_insert_with(|| Table {
                bytes: vec![0; texture.size.height as usize * pitch],
                buffer: None,
                texture_current: false,
            });
        let width = rect.width() as usize * 16;
        let source_pitch = stride.map_or(width, |n| n as usize);
        for row in 0..rect.height() as usize {
            let offset = (rect.min.y as usize + row) * pitch + rect.min.x as usize * 16;
            table.bytes[offset..offset + width]
                .copy_from_slice(&data[row * source_pitch..row * source_pitch + width]);
        }
        table.buffer = None;
        table.texture_current = false;
        true
    }

    pub(super) fn bind_tables(&mut self) -> Result<()> {
        self.gpu.buffer_tables = self.gpu.allow_buffer_tables
            && storage_table_sizes_supported(
                &self.gpu.owner.capabilities.limits,
                self.draw_data.values().filter_map(|texture| {
                    self.tables
                        .get(&texture.allocation_id)
                        .map(|t| t.bytes.len())
                }),
            );
        self.gpu.data_buffers.clear();
        let mut entries: Vec<_> = self.draw_data.iter().collect();
        entries.sort_by_key(|(_, texture)| {
            std::cmp::Reverse(
                self.tables
                    .get(&texture.allocation_id)
                    .map_or(0, |table| table.bytes.len()),
            )
        });
        for (&name, texture) in entries {
            let Some(table) = self.tables.get_mut(&texture.allocation_id) else {
                continue;
            };
            if self.gpu.buffer_tables {
                if table.buffer.is_none() {
                    table.buffer = Some(self.gpu.data_buffer(&table.bytes)?);
                    self.stats.data_table_uploads += 1;
                }
                self.gpu
                    .data_buffers
                    .insert(name, table.buffer.as_ref().unwrap().clone());
            } else if !table.texture_current {
                texture.upload_recorded(
                    &self.gpu.owner,
                    &self.gpu.submissions,
                    DeviceIntRect::from_size(DeviceIntSize::new(
                        texture.size.width as i32,
                        texture.size.height as i32,
                    )),
                    &table.bytes,
                    None,
                    0,
                    None,
                )?;
                table.texture_current = true;
                self.stats.data_table_uploads += 1;
                self.stats.data_table_copies += 1;
            }
        }
        if self.gpu.buffer_tables {
            let mut commands = self.gpu.submissions.recording()?;
            Buffer::transition_many(
                &mut commands,
                self.gpu
                    .data_buffers
                    .values()
                    .map(|b| (b, wgt::BufferUses::STORAGE_READ_ONLY)),
            );
        }
        Ok(())
    }
}
