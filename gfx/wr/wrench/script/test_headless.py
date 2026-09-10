#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import contextlib
import io
import os
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name('headless.py').resolve()


class HeadlessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'wrench').mkdir()
        self.osmesa = self.root / 'target/release/build/osmesa-src-test/out'
        self.osmesa.mkdir(parents=True)
        self.icd = self.root / 'test-icd.json'
        self.icd.write_text('{}')
        self.calls = []

    def run_launcher(self, args, env=None):
        previous = os.getcwd()
        try:
            os.chdir(self.root / 'wrench')
            with patch.dict(os.environ, env or {}, clear=True), \
                    patch('sys.argv', [str(SCRIPT)] + args), \
                    patch('subprocess.check_call', side_effect=lambda cmd, **kw:
                          self.calls.append((cmd, dict(os.environ)))), \
                    contextlib.redirect_stdout(io.StringIO()):
                runpy.run_path(str(SCRIPT), run_name='__main__')
        finally:
            os.chdir(previous)

    def test_default_gl(self):
        self.run_launcher(['reftest'])
        self.assertEqual(self.calls[0][0],
                         ['cargo', 'build', '--verbose', '--features', 'headless', '--release'])
        command, env = self.calls[1]
        self.assertEqual(command, ['../target/release/wrench', '--no-scissor', '--headless', 'reftest'])
        self.assertEqual(env['GALLIUM_DRIVER'], 'llvmpipe')
        self.assertEqual(env['LD_LIBRARY_PATH'],
                         str(self.osmesa / 'mesa/src/gallium/targets/osmesa').replace(str(self.root), '..'))

    def test_hal_avoids_osmesa_and_selects_icd(self):
        args = ['--backend', 'hal', '--hal-backend', 'vulkan', '--hal-validation', 'test_hal']
        self.run_launcher(args, {'WRENCH_VULKAN_ICD': str(self.icd),
                                 'LD_LIBRARY_PATH': '/producer/libs', 'CARGOFLAGS': '--locked -j 2'})
        self.assertIn('hal-vulkan', self.calls[0][0])
        command, env = self.calls[1]
        self.assertEqual(command, ['../target/release/wrench', '--headless'] + args)
        self.assertEqual(env['VK_DRIVER_FILES'], str(self.icd))
        self.assertEqual(env['LD_LIBRARY_PATH'], '/producer/libs')
        self.assertNotIn('GALLIUM_DRIVER', env)
        self.assertNotIn('MESA_GLSL_CACHE_DIR', env)

    def test_prebuilt_hal_debug(self):
        self.run_launcher(['--backend=hal', 'test_init'],
                          {'WRENCH_HEADLESS_TARGET': '/prebuilt/', 'OPTIMIZED': '0'})
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(self.calls[0][0],
                         ['/prebuilt/debug/wrench', '--headless', '--backend=hal', 'test_init'])

    def test_bad_icd_fails_before_build(self):
        with self.assertRaisesRegex(SystemExit, 'existing ICD manifest'):
            self.run_launcher(['--backend', 'hal', 'test_hal'],
                              {'WRENCH_VULKAN_ICD': str(self.root / 'missing.json')})
        self.assertEqual(self.calls, [])


if __name__ == '__main__':
    unittest.main()
