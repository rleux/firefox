# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import textwrap
import unittest

import test_checks_configure
from buildconfig import topsrcdir
from mozunit import main


class TestWebRenderShaderTools(unittest.TestCase):
    tools = {
        "GLSLANG_VALIDATOR": ("glslangValidator", "Glslang Version: 11:15.1.0"),
        "SPIRV_VAL": ("spirv-val", "SPIRV-Tools v2025.1"),
        "SPIRV_DIS": ("spirv-dis", "SPIRV-Tools v2025.1"),
    }

    def configure(
        self,
        kernel="Linux",
        os_name="GNU",
        compile=True,
        missing=None,
        old=None,
        override=False,
    ):
        tools_dir = os.path.abspath("/shader-tools/bin")
        paths = {}
        environ = {"PATH": os.path.abspath("/empty")}
        for variable, (program, version_output) in self.tools.items():
            if variable == missing:
                continue
            output = version_output
            if variable == old:
                output = output.replace("15.1.0", "14.0.0").replace("2025.1", "2024.1")
            path = os.path.join(tools_dir, program)
            if override and variable == "GLSLANG_VALIDATOR":
                path = os.path.abspath("/custom/glslangValidator")
                environ[variable] = path
            paths[path] = lambda stdin, args, output=output: (0, output, "")
        script = textwrap.dedent(f"""
            @depends("--help")
            @imports(_from="mozbuild.util", _import="ReadOnlyNamespace", _as="namespace")
            def target(_):
                return namespace(kernel={kernel!r}, os={os_name!r})
            compile_environment = dependable({compile!r})

            @template
            def bootstrap_search_path(path, paths=None, when=None):
                assert path == "shader-tools/bin"
                return dependable([{tools_dir!r}])

            original_check_prog = check_prog

            @template
            def check_prog(*args, **kwargs):
                return original_check_prog(*args, bootstrap_search_path=bootstrap_search_path, **kwargs)

            include({os.path.join(topsrcdir, "build/moz.configure/webrender.configure")!r})
        """)
        return test_checks_configure.TestChecksConfigure().get_result(
            script, environ=environ, extra_paths=paths
        )

    def test_bootstrapped_tools_without_path(self):
        config, output, status = self.configure()
        self.assertEqual(status, 0, output)
        for variable, (program, _) in self.tools.items():
            self.assertEqual(
                config[variable], os.path.abspath(f"/shader-tools/bin/{program}")
            )
            self.assertIn(f"{variable}_VERSION", config)

    def test_explicit_tool_override(self):
        config, output, status = self.configure(override=True)
        self.assertEqual(status, 0, output)
        self.assertEqual(
            config["GLSLANG_VALIDATOR"], os.path.abspath("/custom/glslangValidator")
        )

    def test_missing_tools(self):
        for variable in self.tools:
            with self.subTest(tool=variable):
                _, output, status = self.configure(missing=variable)
                self.assertEqual(status, 1)
                self.assertIn(f"set {variable} to its full path", output)

    def test_old_tools(self):
        for variable in self.tools:
            with self.subTest(tool=variable):
                _, output, status = self.configure(old=variable)
                self.assertEqual(status, 1)
                self.assertIn(f"requires {self.tools[variable][0]}", output)

    def test_unneeded_tools(self):
        for kwargs in [
            {"compile": False},
            {"os_name": "Android"},
            {"kernel": "Darwin", "os_name": "OSX"},
            {"kernel": "WINNT", "os_name": "WINNT"},
        ]:
            with self.subTest(**kwargs):
                config, output, status = self.configure(
                    missing="GLSLANG_VALIDATOR", **kwargs
                )
                self.assertEqual(status, 0, output)
                self.assertEqual(config, {})


if __name__ == "__main__":
    main()
