/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::surface::SurfaceSetup;
use std::rc::Rc;
#[cfg(feature = "hal-translate")]
use std::collections::HashMap;
use webrender_build::hal::ShaderArtifact;

pub(super) mod sealed {
    pub trait Sealed {}
}

pub(crate) trait BackendApi: hal::Api + sealed::Sealed {
    fn create_device(
        options: &Options,
        window: Option<Rc<dyn SurfaceWindow>>,
    ) -> Result<(Device<Self>, Option<SurfaceSetup<Self>>)>;
    fn open_adapter(adapter: &hal::ExposedAdapter<Self>, features: wgt::Features)
        -> Result<(hal::OpenDevice<Self>, wgt::Features)> {
        let open = unsafe { adapter.adapter.open(features, &adapter.capabilities.limits, &wgt::MemoryHints::default()) }
            .map_err(|error| format!("Opening {}: {error:?}", adapter.info.name))?;
        Ok((open, features))
    }
    #[cfg(wr_hal_metal)]
    fn create_metal_layer_surface(_instance: &Self::Instance, _layer: &objc2_quartz_core::CAMetalLayer) -> Result<Self::Surface> {
        Err("This backend cannot create a direct Metal-layer surface".into())
    }
    fn timestamp_valid_bits(device: &Device<Self>) -> u32;
    fn shader_input() -> Result<ShaderInputMode>;
    fn create_shader_module(
        device: &Self::Device,
        artifact: &ShaderArtifact,
        fragment: bool,
        mode: ShaderInputMode,
        cache: &mut ShaderCache,
    ) -> Result<Self::ShaderModule>;
    fn supports_presentation_blit(_device: &Device<Self>, _format: wgt::TextureFormat) -> bool { false }
    unsafe fn record_presentation_blit(
        _device: &Self::Device,
        _encoder: &mut Self::CommandEncoder,
        _source: &Self::Texture,
        _target: &Self::Texture,
        _source_size: [u32; 2],
        _target_size: [u32; 2],
    ) -> Result<()> { Err("Native presentation blit is unavailable".into()) }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ShaderInputMode {
    Native,
    #[cfg(feature = "hal-translate")]
    Naga,
}

impl ShaderInputMode {
    pub(crate) fn from_env() -> Result<Self> {
        match std::env::var("WR_HAL_SHADER_INPUT") {
            Err(std::env::VarError::NotPresent) => Ok(Self::Native),
            Ok(value) if value == "native" => Ok(Self::Native),
            #[cfg(feature = "hal-translate")]
            Ok(value) if value == "naga" => Ok(Self::Naga),
            Ok(value) => Err(format!(
                "Unsupported HAL shader input {value:?}; naga requires hal-naga"
            )),
            Err(error) => Err(format!("Invalid HAL shader input: {error}")),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Native => "native-spirv",
            #[cfg(feature = "hal-translate")]
            Self::Naga => "naga30-matrix-io-fetch-offset-v1",
        }
    }
}

#[derive(Default)]
pub(crate) struct ShaderCache {
    #[cfg(feature = "hal-translate")]
    translated: HashMap<&'static [u8], webrender_build::hal::translate::ValidatedShader>,
}

impl ShaderCache {
    pub(super) fn create_module<A: hal::Api>(
        &mut self,
        device: &A::Device,
        artifact: &ShaderArtifact,
        fragment: bool,
        mode: ShaderInputMode,
    ) -> Result<A::ShaderModule> {
        let data = if fragment {
            artifact.fragment
        } else {
            artifact.vertex
        };
        let words: Vec<_> = data
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        let input = match mode {
            ShaderInputMode::Native => hal::ShaderInput::SpirV(&words),
            #[cfg(feature = "hal-translate")]
            ShaderInputMode::Naga => {
                if !self.translated.contains_key(data) {
                    let shader = webrender_build::hal::translate::parse_spirv(data)?;
                    self.translated.insert(data, shader);
                }
                let shader = &self.translated[data];
                hal::ShaderInput::Naga(hal::NagaShader {
                    module: std::borrow::Cow::Owned(shader.module.clone()),
                    info: shader.info.clone(),
                    debug_source: None,
                })
            }
        };
        unsafe {
            device.create_shader_module(
                &hal::ShaderModuleDescriptor {
                    label: Some(artifact.name),
                    runtime_checks: wgt::ShaderRuntimeChecks::default(),
                },
                input,
            )
        }
        .map_err(|error| format!("Creating shader {}: {error:?}", artifact.name))
    }
}

#[cfg(all(test, wr_hal_vulkan, feature = "hal-translate"))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires Vulkan"]
    fn translated_cache_uses_content_instead_of_supplied_digest() {
        let owner = create_vulkan_device(&Options { validation: true, ..Default::default() }).unwrap();
        let shaders = super::super::render::shader_catalog_for_test();
        let first = &shaders[0];
        let second = shaders.iter().find(|shader| shader.vertex != first.vertex).unwrap();
        let mut cache = ShaderCache::default();
        for artifact in [first, second, first] {
            let collision = ShaderArtifact {
                name: artifact.name, features: artifact.features, vertex: artifact.vertex,
                fragment: artifact.fragment, inputs: artifact.inputs, textures: artifact.textures,
                projection_stages: artifact.projection_stages, digest: 0,
            };
            let module = cache.create_module::<hal::api::Vulkan>(&owner.open.device,
                &collision, false, ShaderInputMode::Naga).unwrap();
            unsafe { owner.open.device.destroy_shader_module(module); }
        }
        assert_eq!(cache.translated.len(), 2);
    }
}
