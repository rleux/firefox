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
        || args.subcommand_name() == Some("test_hal")
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

#[cfg(not(feature = "hal-vulkan"))]
fn run(_: &clap::ArgMatches) -> Result<(), String> {
    Err("Vulkan support is not compiled in; build Wrench with --features hal-vulkan".into())
}

#[cfg(feature = "hal-vulkan")]
fn run(args: &clap::ArgMatches) -> Result<(), String> {
    use webrender::hal::{create_vulkan_device, Options, Readback};

    if !args.is_present("headless") {
        return Err("HAL bootstrap requires --headless".into());
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
            return Err(format!("--{option} is incompatible with the HAL bootstrap"));
        }
    }
    let command = args.subcommand_name().unwrap_or("");
    if !matches!(command, "test_init" | "test_hal") {
        return Err(format!(
            "{command:?} is not implemented for HAL yet; use test_init or test_hal"
        ));
    }
    let dimensions = match args.value_of("size") {
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
