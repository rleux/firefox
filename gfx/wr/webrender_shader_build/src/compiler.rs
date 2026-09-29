/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::reflection::Reflection;
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(feature = "glslang")]
#[path = "glslang.rs"]
mod glslang;

#[cfg(feature = "naga")]
#[path = "naga.rs"]
mod naga;

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
            #[cfg(feature = "naga")]
            naga::COMPILER,
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
