/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::Compiler;
use crate::reflection::{self, Reflection};
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const COMPILER: Compiler = Compiler {
    name: "glslang",
    environment: &[
        "GLSLANG_VALIDATOR",
        "SPIRV_VAL",
        "SPIRV_DIS",
        "MOZ_FETCHES_DIR",
    ],
    preprocess,
    compile,
    link,
    reflect,
};

fn tool(variable: &str, name: &str) -> OsString {
    std::env::var_os(variable).unwrap_or_else(|| {
        if let Some(fetches) = std::env::var_os("MOZ_FETCHES_DIR") {
            let name = if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_owned()
            };
            PathBuf::from(fetches)
                .join("shader-tools/bin")
                .join(name)
                .into_os_string()
        } else {
            name.into()
        }
    })
}

fn run(command: &mut Command) -> io::Result<String> {
    let output = command
        .output()
        .map_err(|error| io::Error::other(format!("{command:?}: {error}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{command:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}

fn preprocess(source: &Path) -> io::Result<String> {
    run(Command::new(tool("GLSLANG_VALIDATOR", "glslangValidator"))
        .arg("-E")
        .arg(source))
}

fn compile(source: &Path, output: &Path) -> io::Result<()> {
    run(Command::new(tool("GLSLANG_VALIDATOR", "glslangValidator"))
        .args(["-V", "-Os", "--target-env", "vulkan1.1", "-o"])
        .arg(output)
        .arg(source))?;
    run(Command::new(tool("SPIRV_VAL", "spirv-val"))
        .args(["--target-env", "vulkan1.1"])
        .arg(output))?;
    Ok(())
}

fn link(sources: &[PathBuf], directory: &Path) -> io::Result<()> {
    let output = directory.join("linked-validation.spv");
    run(Command::new(tool("GLSLANG_VALIDATOR", "glslangValidator"))
        .args(["-V", "--target-env", "vulkan1.1", "-l", "-o"])
        .arg(&output)
        .args(sources))?;
    fs::remove_file(output)
}

fn reflect(binary: &Path) -> io::Result<Reflection> {
    run(Command::new(tool("SPIRV_VAL", "spirv-val"))
        .args(["--target-env", "vulkan1.1"])
        .arg(binary))?;
    Ok(reflection::reflect(&run(Command::new(tool(
        "SPIRV_DIS",
        "spirv-dis",
    ))
    .arg("--raw-id")
    .arg(binary))?))
}
