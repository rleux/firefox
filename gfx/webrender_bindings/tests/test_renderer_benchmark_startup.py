# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import tempfile
import unittest
from pathlib import Path

from renderer_benchmark_startup import wait_for_startup


class FakeSampler:
    interval = 0.25

    def __init__(self, proc_root, age=60, children=None):
        self.proc_root = proc_root
        self.age = age
        self.now = 0
        self.children = children or (lambda _: {})
        self.root = "10:100"
        self.calls = 0

    def sleep(self, duration):
        self.now += duration

    def sample_once(self, detail):
        if detail != "light":
            raise AssertionError("Startup must not collect heavy metrics")
        self.calls += 1
        (self.proc_root / "uptime").write_text(
            f"{100 / os.sysconf('SC_CLK_TCK') + self.age + self.now} 0\n"
        )
        return {
            "rootIdentity": self.root,
            "processes": {
                self.root: {"startTimeTicks": 100, "comm": "firefox"},
                **self.children(self.now),
            },
        }


class StartupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.sampler = FakeSampler(Path(self.directory.name))
        self.report = {}

    def wait(self, **limits):
        wait_for_startup(
            self.sampler,
            self.report,
            clock=lambda: self.sampler.now,
            sleep=self.sampler.sleep,
            **limits,
        )

    def test_minimum_age_then_stability(self):
        self.wait()
        self.assertTrue(self.report["passed"])
        self.assertEqual(self.report["elapsedSeconds"], 10)
        self.assertEqual(self.report["rootAgeSeconds"], 70)
        self.assertEqual(self.report["stableSeconds"], 5)
        self.assertEqual(len(self.report["transitions"]), 1)

    def test_child_departure_restarts_stability(self):
        self.sampler.children = lambda now: (
            {"11:101": {"comm": "crashreporter"}} if 6 <= now < 8 else {}
        )
        self.wait()
        self.assertEqual(self.report["elapsedSeconds"], 13)
        transitions = self.report["transitions"]
        self.assertEqual(transitions[1]["added"], {"11:101": "crashreporter"})
        self.assertEqual(transitions[2]["removed"], {"11:101": "crashreporter"})

    def test_pid_reuse_restarts_stability(self):
        self.sampler.age = 70
        self.sampler.children = lambda now: {
            f"11:{101 if now < 4 else 102}": {"comm": "content"}
        }
        self.wait()
        self.assertEqual(self.report["elapsedSeconds"], 9)
        self.assertIn("11:102", self.report["finalIdentities"])

    def test_root_replacement_fails(self):
        def replace_root(duration):
            self.sampler.sleep(duration)
            self.sampler.root = "10:102"

        with self.assertRaisesRegex(RuntimeError, "root process identity"):
            wait_for_startup(
                self.sampler,
                self.report,
                clock=lambda: self.sampler.now,
                sleep=replace_root,
            )
        self.assertFalse(self.report["passed"])
        self.assertIn("identity", self.report["error"])

    def test_timeout_preserves_evidence(self):
        self.sampler.children = lambda now: {f"11:{now}": {"comm": "content"}}
        with self.assertRaises(TimeoutError):
            self.wait(timeout=12)
        self.assertFalse(self.report["passed"])
        self.assertEqual(self.report["elapsedSeconds"], 12)
        self.assertGreater(len(self.report["transitions"]), 2)

    def test_invalid_age_fails(self):
        self.sampler.age = float("nan")
        with self.assertRaisesRegex(RuntimeError, "Invalid startup process age"):
            self.wait()

    def test_old_root_still_requires_stability(self):
        self.sampler.age = 300
        self.wait()
        self.assertEqual(self.report["elapsedSeconds"], 5)


if __name__ == "__main__":
    unittest.main()
