/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use ash::vk;

#[test]
fn shader_api_version_excludes_older_adapters_before_ranking() {
    assert!(!supports_shader_api_version(vk::API_VERSION_1_0));
    assert!(!supports_shader_api_version(vk::make_api_version(0, 1, 0, 255)));
    assert!(supports_shader_api_version(vk::API_VERSION_1_1));
    assert!(supports_shader_api_version(vk::API_VERSION_1_2));
    let mut adapters = vec![
        ("old discrete", wgt::DeviceType::DiscreteGpu, vk::API_VERSION_1_0),
        ("integrated", wgt::DeviceType::IntegratedGpu, vk::API_VERSION_1_1),
        ("software", wgt::DeviceType::Cpu, vk::API_VERSION_1_2),
    ];
    adapters.retain(|adapter| supports_shader_api_version(adapter.2));
    for name in [None, Some("old discrete"), Some("integrated")] {
        let index = select_adapter(adapters.iter().map(|a| (a.0, a.1)), name).unwrap();
        assert_eq!(adapters[index].0, "integrated");
    }
}
