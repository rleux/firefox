/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::reflection::Reflection;
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(feature = "glslang")]
use std::{ffi::OsString, process::Command, sync::OnceLock};

#[cfg(feature = "glslang")]
#[path = "glslang.rs"]
mod glslang;

#[derive(Clone, Copy)]
pub struct Compiler {
    name: &'static str,
    environment: &'static [&'static str],
    preprocess: fn(&Path) -> io::Result<String>,
    compile: fn(&Path, &Path) -> io::Result<()>,
    link: fn(&[PathBuf], &Path) -> io::Result<()>,
    reflect: fn(&Path) -> io::Result<Reflection>,
}

impl Compiler {
    pub fn available() -> &'static [Self] {
        &[
            #[cfg(feature = "glslang")]
            glslang::COMPILER,
        ]
    }

    pub fn from_name(name: &str) -> io::Result<Self> {
        Self::available()
            .iter()
            .find(|compiler| compiler.name == name)
            .copied()
            .ok_or_else(|| {
                io::Error::other(format!("Vulkan shader compiler {name:?} is not enabled"))
            })
    }

    pub fn name(self) -> &'static str {
        self.name
    }

    pub(crate) fn rerun_if_changed(self) {
        for variable in self.environment {
            println!("cargo:rerun-if-env-changed={variable}");
        }
    }

    pub(crate) fn preprocess(self, source: &Path) -> io::Result<String> {
        (self.preprocess)(source)
    }

    pub fn compile(self, source: &Path, output: &Path) -> io::Result<()> {
        (self.compile)(source, output)
    }

    pub(crate) fn link(self, sources: &[PathBuf], directory: &Path) -> io::Result<()> {
        (self.link)(sources, directory)
    }

    pub(crate) fn reflect(self, binary: &Path) -> io::Result<Reflection> {
        (self.reflect)(binary)
    }
}

#[test]
fn compiler_choice_does_not_silently_fall_back() {
    for compiler in Compiler::available() {
        assert_eq!(
            Compiler::from_name(compiler.name()).unwrap().name(),
            compiler.name()
        );
    }
    for invalid in ["", "auto", "unknown"] {
        assert!(Compiler::from_name(invalid).is_err());
    }
}

#[cfg(feature = "glslang")]
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

#[cfg(feature = "glslang")]
pub(crate) fn validator_tool() -> io::Result<OsString> {
    static VALIDATOR: OnceLock<Result<OsString, String>> = OnceLock::new();
    VALIDATOR
        .get_or_init(|| {
            let executable = tool("SPIRV_VAL", "spirv-val");
            let output = Command::new(&executable)
                .arg("--version")
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!("{executable:?} --version failed"));
            }
            let version = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
            let release = version
                .split_whitespace()
                .find_map(|word| word.strip_prefix('v'))
                .and_then(|number| {
                    let mut components = number.split('.');
                    Some((
                        components.next()?.parse::<u32>().ok()?,
                        components.next()?.parse::<u32>().ok()?,
                    ))
                });
            // Older validators accept invalid explicit layouts in Function storage.
            if release.is_none_or(|release| release < (2026, 1)) {
                return Err(format!(
                    "SPIRV_VAL must be SPIRV-Tools 2026.1 or newer; got {version}"
                ));
            }
            Ok(executable)
        })
        .clone()
        .map_err(io::Error::other)
}
