//! Ordinary Linux DMA-BUF imports without native handles.

use alloc::string::String;

#[derive(Clone, Debug)]
pub struct DmaBufFormat {
    pub(crate) format: wgt::TextureFormat,
    pub(crate) usage: wgt::TextureUses,
    pub(crate) modifier: u64,
    pub(crate) max_size: [u32; 2],
    pub(crate) max_resource_size: u64,
    pub(crate) exportable: bool,
    pub(crate) dedicated_only: bool,
}

impl DmaBufFormat {
    pub fn format(&self) -> wgt::TextureFormat {
        self.format
    }
    pub fn usage(&self) -> wgt::TextureUses {
        self.usage
    }
    pub fn modifier(&self) -> u64 {
        self.modifier
    }
    pub fn max_size(&self) -> [u32; 2] {
        self.max_size
    }
    pub fn max_resource_size(&self) -> u64 {
        self.max_resource_size
    }
    pub fn exportable(&self) -> bool {
        self.exportable
    }
    pub fn dedicated_only(&self) -> bool {
        self.dedicated_only
    }

    /// Checks dimensions; allocation requirements still need validation after image creation.
    pub fn supports_extent(&self, size: [u32; 2]) -> bool {
        size[0] != 0 && size[1] != 0 && size[0] <= self.max_size[0] && size[1] <= self.max_size[1]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DmaBufImageDescriptor {
    pub size: [u32; 2],
    pub format: wgt::TextureFormat,
    pub usage: wgt::TextureUses,
    pub modifier: u64,
    pub offset: u64,
    pub row_pitch: u64,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

impl DmaBufImageDescriptor {
    pub fn validate_plane(&self, bytes: u64) -> Result<(), String> {
        if self.size.contains(&0)
            || self.size.iter().any(|&size| size > i32::MAX as u32)
            || self.row_pitch == 0
            || self.offset >= bytes
        {
            return Err("Invalid DMA-BUF dimensions, pitch or offset".into());
        }
        if self.modifier == 0 {
            let row = u64::from(self.size[0]) * 4;
            if self.row_pitch < row || self.row_pitch % 4 != 0 || self.offset % 4 != 0 {
                return Err("Invalid linear RGB DMA-BUF layout".into());
            }
            let end = self
                .row_pitch
                .checked_mul(u64::from(self.size[1] - 1))
                .and_then(|value| value.checked_add(row))
                .and_then(|value| value.checked_add(self.offset))
                .ok_or("Linear DMA-BUF layout overflow")?;
            if end > bytes {
                return Err("Linear DMA-BUF pixels exceed the allocation".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalDomain {
    External,
    Foreign,
}

#[cfg(all(vulkan, target_os = "linux"))]
pub use crate::vulkan::dma_buf::{query_capabilities, supports_imports, Capabilities, Image};

#[cfg(all(vulkan, target_os = "linux", feature = "validation_canary"))]
pub use crate::vulkan::dma_buf::testing;
