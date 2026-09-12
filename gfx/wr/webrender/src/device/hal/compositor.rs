/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{ExternalImageDevice, ExternalImageLease};
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use crate::composite::{CompositeDescriptor, CompositorCapabilities, CompositorInputConfig, CompositorKind, NativeSurfaceOperation, NativeTileId};
use crate::renderer::hal::FrameCompletion;

pub struct CompositorTarget {
    pub image: ExternalImageLease,
    pub origin: DeviceIntPoint,
    pub size: DeviceIntSize,
}

pub trait NativeCompositor {
    fn update_surfaces(&mut self, device: &ExternalImageDevice, operations: &[NativeSurfaceOperation]) -> Result<(), String>;
    fn bind_tile(&mut self, id: NativeTileId, dirty: DeviceIntRect, valid: DeviceIntRect) -> Result<CompositorTarget, String>;
    fn read_tile(&mut self, id: NativeTileId) -> Result<CompositorTarget, String>;
    fn end_frame(&mut self, descriptor: &CompositeDescriptor, completion: FrameCompletion) -> Result<(), String>;
}

pub trait LayerCompositor {
    fn begin_frame(&mut self, device: &ExternalImageDevice, config: &CompositorInputConfig) -> Result<(), String>;
    fn bind_layer(&mut self, index: usize, dirty: &[DeviceIntRect]) -> Result<CompositorTarget, String>;
    fn end_frame(&mut self, completion: FrameCompletion) -> Result<(), String>;
}

pub enum CompositorConfig {
    Draw,
    Native { capabilities: CompositorCapabilities, compositor: Box<dyn NativeCompositor> },
    Layer { compositor: Box<dyn LayerCompositor> },
}

impl Default for CompositorConfig {
    fn default() -> Self { Self::Draw }
}

impl CompositorConfig {
    pub(crate) fn kind(&self) -> CompositorKind {
        match self {
            Self::Draw => CompositorKind::default(),
            Self::Native { capabilities, .. } => CompositorKind::Native { capabilities: *capabilities },
            Self::Layer { .. } => CompositorKind::Layer {},
        }
    }
}
