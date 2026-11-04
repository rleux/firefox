/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{fs, process::Command};

#[test]
fn actual_core_timestamp_shader_produces_valid_internal_spirv() {
    let source = format!("{}\n{}",
        include_str!("../../../../third_party/rust/wgpu-core/src/timestamp_normalization/common.wgsl"),
        include_str!("../../../../third_party/rust/wgpu-core/src/timestamp_normalization/timestamp_normalization.wgsl"));
    let module = naga::front::wgsl::parse_str(&source).unwrap();
    let info = wgpu_naga_bridge::create_validator(
        wgpu_types::Features::IMMEDIATES,
        wgpu_types::DownlevelFlags::empty(),
        naga::valid::ValidationFlags::all(),
    )
    .validate(&module)
    .unwrap();
    let directory = std::env::temp_dir().join(format!("wr-core-timestamp-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let validator = crate::compiler::validator_tool().unwrap();
    for period in [80.0_f32, 83.333333_f32] {
        let shift = 32 - (period.log2().ceil() as i32).clamp(-32, 32).unsigned_abs();
        let multiply = (period as f64 * (1_u64 << shift) as f64).round() as u32;
        let constants = [
            ("TIMESTAMP_PERIOD_MULTIPLY".to_owned(), multiply as f64),
            ("TIMESTAMP_PERIOD_SHIFT".to_owned(), shift as f64),
        ]
        .into_iter()
        .collect();
        let (specialized, info) = naga::back::pipeline_constants::process_overrides(
            &module,
            &info,
            Some((naga::ShaderStage::Compute, "main")),
            &constants,
        )
        .unwrap();
        for debug in [false, true] {
            // Match core's trusted internal shader profile.
            let mut options = naga::back::spv::Options::default();
            options.lang_version = (1, 6);
            options.flags = naga::back::spv::WriterFlags::LABEL_VARYINGS
                | naga::back::spv::WriterFlags::FORCE_POINT_SIZE;
            options
                .flags
                .set(naga::back::spv::WriterFlags::DEBUG, debug);
            options.bounds_check_policies = naga::proc::BoundsCheckPolicies {
                index: naga::proc::BoundsCheckPolicy::Unchecked,
                buffer: naga::proc::BoundsCheckPolicy::Unchecked,
                image_load: naga::proc::BoundsCheckPolicy::Unchecked,
                binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
            };
            options.force_loop_bounding = false;
            options.ray_query_initialization_tracking = false;
            options.emit_int_div_checks = false;
            options.zero_initialize_workgroup_memory =
                naga::back::spv::ZeroInitializeWorkgroupMemoryMode::None;
            let words = naga::back::spv::write_vec(
                &specialized,
                &info,
                &options,
                Some(&naga::back::spv::PipelineOptions {
                    shader_stage: naga::ShaderStage::Compute,
                    entry_point: "main".into(),
                }),
            )
            .unwrap();
            let path = directory.join(format!("period-{period}-debug-{debug}.spv"));
            fs::write(
                &path,
                words
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let result = Command::new(&validator)
                .args(["--relax-block-layout", "--target-env", "vulkan1.3"])
                .arg(&path)
                .output()
                .expect("SPIRV_VAL must name the pinned external validator");
            assert!(
                result.status.success(),
                "period={period} debug={debug} {}: {}",
                path.display(),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    fs::remove_dir_all(directory).unwrap();
}
