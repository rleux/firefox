# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from copy import deepcopy
from pathlib import Path

from renderer_benchmark_metrics import (
    HAL_COUNTERS,
    HAL_GAUGES,
    Sampler,
    validate_environment,
    validate_report,
)
from run_renderer_benchmark import stop


def stat_line(pid, parent, start, user=10, system=5, rss=3, comm="test process"):
    fields = ["S", str(parent)] + ["0"] * 9
    fields += [str(user), str(system)] + ["0"] * 6
    fields += [str(start), "0", str(rss)]
    return f"{pid} ({comm}) " + " ".join(fields) + "\n"


def valid_report():
    unavailable = {
        "available": False,
        "values": {},
        "errors": {},
        "reason": "not-available",
    }
    process = {
        "pid": 10,
        "startTimeTicks": 100,
        "userCpuSeconds": 1.0,
        "systemCpuSeconds": 0.5,
        "cpuSeconds": 1.5,
        "rssBytes": 4096,
        "fdCount": 3,
        "fdInfoCoverage": 3,
        "errors": [],
    }
    sample = {
        "timeSeconds": 1.0,
        "rootIdentity": "10:100",
        "processes": {"10:100": process},
        "totals": {
            "cpuSeconds": 1.5,
            "userCpuSeconds": 1.0,
            "systemCpuSeconds": 0.5,
            "rssBytes": 4096,
            "fdCount": 3,
            "fdCoverage": 1,
            "fdInfoCoverage": 3,
            "pssBytes": 2048,
            "privateBytes": 1024,
            "memoryCoverage": 1,
        },
        "drmClients": {},
        "host": {
            "wholeHostCpuTicks": {
                "available": True,
                "value": {"user": 1, "system": 1, "idle": 10},
                "error": None,
                "reason": None,
            },
            "loadAverage": {
                "available": True,
                "value": {
                    "oneMinute": 0.1,
                    "fiveMinutes": 0.2,
                    "fifteenMinutes": 0.3,
                    "runnable": 1,
                    "processes": 100,
                },
                "error": None,
                "reason": None,
            },
            "backgroundProcessCpu": {
                "available": True,
                "processCount": 1,
                "cpuSeconds": 2.0,
                "reason": None,
            },
            "cpuClocksKHz": unavailable,
            "cpuGovernors": unavailable,
            "temperaturesMilliC": unavailable,
            "power": unavailable,
            "platformProfile": {
                "available": False,
                "value": None,
                "error": "missing",
                "reason": "not-available",
            },
        },
    }
    return {
        "schemaVersion": 1,
        "passed": True,
        "backend": {
            "backend": "Vulkan (wgpu-hal)",
            "renderer": "Intel Xe Graphics",
            "driver": "Mesa Intel",
            "process": "GPU",
        },
        "presentation": {"mode": "native", "wsiDebug": None},
        "geometryBefore": {
            "viewport": [900, 700],
            "dpr": 1,
            "visible": True,
            "focused": True,
        },
        "geometryAfter": {
            "viewport": [900, 700],
            "dpr": 1,
            "visible": True,
            "focused": True,
        },
        "workload": {
            "name": "css-animation",
            "requestedDurationMs": 1000,
            "durationMs": 1001,
            "completed": True,
            "samples": [16.5, 16.7],
            "updates": 60,
            "events": [],
        },
        "processMetrics": [sample, {**sample, "timeSeconds": 2.0}],
        "hostIntervalStart": 0.9,
        "hostIntervalEnd": 2.1,
        "diagnostics": [],
        "runtime": {
            "binary": {"path": "/snapshot/firefox", "sha256": "b" * 64},
            "libxul": {"path": "/snapshot/libxul.so", "sha256": "a" * 64},
        },
        "mappingsBefore": ["/snapshot/libxul.so", "/system/libvulkan.so.1"],
        "mappingsAfter": ["/snapshot/libxul.so", "/system/libvulkan.so.1"],
        "libraryHashesBefore": {
            "/snapshot/libxul.so": "a" * 64,
            "/system/libvulkan.so.1": "c" * 64,
        },
        "libraryHashesAfter": {
            "/snapshot/libxul.so": "a" * 64,
            "/system/libvulkan.so.1": "c" * 64,
        },
        "pixels": {
            "size": [900, 700],
            "pixels": [[220, 40, 60, 255], [20, 180, 80, 255]],
            "checkpoint": {
                "points": [[8, 8], [96, 96]],
                "expected": [[220, 40, 60, 255], [20, 180, 80, 255]],
                "frames": 60,
                "scrollTop": 0,
            },
        },
    }


def expected(phase="timing"):
    return {
        "backend": "vulkan",
        "renderer": "Intel Xe",
        "process": "GPU",
        "viewport": [900, 700],
        "dpr": 1,
        "allowSoftware": False,
        "phase": phase,
        "workload": "css-animation",
        "runtime": {
            "binary": {"path": "/snapshot/firefox", "sha256": "b" * 64},
            "libxul": {"path": "/snapshot/libxul.so", "sha256": "a" * 64},
        },
    }


def diagnostic_record():
    return {
        "version": 1,
        "pid": 1,
        "deviceId": 1,
        "rendererId": 1,
        "sequence": 1,
        "monotonicNs": 1,
        "final": False,
        "counters": {name: 0 for name in HAL_COUNTERS},
        "gauges": {name: 0 for name in HAL_GAUGES},
        "peaks": {name: 0 for name in HAL_GAUGES},
        "lastWorkNs": {name: 0 for name in HAL_COUNTERS},
    }


class TestSampler(unittest.TestCase):
    def test_process_tree_memory_fd_and_drm_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            proc = root / "proc"
            sys = root / "sys"
            for pid, parent, start in [(10, 1, 100), (11, 10, 110), (20, 1, 200)]:
                directory = proc / str(pid)
                (directory / "fdinfo").mkdir(parents=True)
                (directory / "stat").write_text(stat_line(pid, parent, start))
                (directory / "smaps_rollup").write_text(
                    "Pss: 4 kB\nPrivate_Clean: 1 kB\nPrivate_Dirty: 2 kB\n"
                )
            (proc / "10/fdinfo/3").write_text(
                "drm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\ndrm-engine-render:\t42 ns\n"
            )
            (proc / "11/fdinfo/4").write_text(
                "drm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\ndrm-engine-render:\t47 ns\n"
            )
            (proc / "stat").write_text("cpu 1 2 3 4 5 6 7 8 9 10\n")
            (proc / "loadavg").write_text("0.10 0.20 0.30 2/50 123\n")
            governor = sys / "devices/system/cpu/cpu0/cpufreq/scaling_governor"
            governor.parent.mkdir(parents=True)
            governor.write_text("performance\n")
            profile = sys / "firmware/acpi/platform_profile"
            profile.parent.mkdir(parents=True)
            profile.write_text("balanced\n")
            sampler = Sampler(10, True, proc_root=proc, sys_root=sys, clock=lambda: 5.0)
            sample = sampler.sample_once()
            self.assertEqual(set(sample["processes"]), {"10:100", "11:110"})
            self.assertEqual(sample["totals"]["memoryCoverage"], 2)
            self.assertEqual(sample["totals"]["pssBytes"], 8192)
            client = sample["drmClients"]["0000:00:02.0/7"]
            self.assertEqual(client["owners"], ["10:100", "11:110"])
            self.assertEqual(client["firstRaw"]["drm-engine-render"], "42 ns")
            self.assertEqual(client["lastRaw"]["drm-engine-render"], "47 ns")
            self.assertEqual(client["observations"], 2)
            self.assertFalse(sample["host"]["cpuClocksKHz"]["available"])
            self.assertEqual(sample["host"]["backgroundProcessCpu"]["processCount"], 1)
            self.assertEqual(sample["host"]["wholeHostCpuTicks"]["value"]["idle"], 4)
            self.assertEqual(sample["host"]["loadAverage"]["value"]["runnable"], 2)
            self.assertEqual(
                next(iter(sample["host"]["cpuGovernors"]["values"].values())),
                "performance",
            )
            self.assertEqual(sample["host"]["platformProfile"]["value"], "balanced")

    def test_missing_or_reused_root_is_an_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            (proc / "10/fdinfo").mkdir(parents=True)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            sampler.sample_once()
            (proc / "10/stat").write_text(stat_line(10, 1, 101))
            with self.assertRaises(RuntimeError):
                sampler.sample_once()
            with self.assertRaises(ProcessLookupError):
                Sampler(99, proc_root=proc, sys_root=proc).sample_once()

    def test_context_has_synchronous_endpoints(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            (proc / "10/fdinfo").mkdir(parents=True)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            with Sampler(10, interval=10, proc_root=proc, sys_root=proc) as sampler:
                self.assertEqual(len(sampler.samples), 1)
            self.assertEqual(len(sampler.samples), 2)


@unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
class TestOwnedCleanup(unittest.TestCase):
    def test_sigterm_cleans_separate_session_child_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child_pid = root / "child.pid"
            child_ready = root / "child.ready"
            cleaned = root / "cleaned"
            module_path = str(Path(__file__).parent.resolve())
            child_source = """
import pathlib
import signal
import sys
import time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
pathlib.Path(sys.argv[1]).write_text("ready")
time.sleep(60)
"""
            owner_source = f"""
import pathlib
import subprocess
import sys
import time
sys.path.insert(0, {module_path!r})
from run_renderer_benchmark import install_termination_handler, stop
install_termination_handler()
child = subprocess.Popen(
    [sys.executable, "-c", {child_source!r}, {str(child_ready)!r}],
    start_new_session=True,
)
deadline = time.monotonic() + 5
while not pathlib.Path({str(child_ready)!r}).exists() and time.monotonic() < deadline:
    time.sleep(0.01)
if not pathlib.Path({str(child_ready)!r}).exists():
    raise RuntimeError("child did not start")
pathlib.Path({str(child_pid)!r}).write_text(str(child.pid))
try:
    while True:
        time.sleep(1)
finally:
    stop(child)
    pathlib.Path({str(cleaned)!r}).write_text("yes")
"""
            owner = subprocess.Popen(
                [sys.executable, "-c", owner_source], start_new_session=True
            )
            unrelated = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(60)"],
                start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not child_pid.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(child_pid.exists())
                owned_pid = int(child_pid.read_text())
                os.killpg(owner.pid, signal.SIGTERM)
                self.assertEqual(owner.wait(timeout=15), 128 + signal.SIGTERM)
                self.assertTrue(cleaned.exists())
                with self.assertRaises(ProcessLookupError):
                    os.kill(owned_pid, 0)
                self.assertIsNone(unrelated.poll())
            finally:
                stop(owner)
                stop(unrelated)


class TestValidateReport(unittest.TestCase):
    def test_canvas_requires_the_same_software_producer_policy(self):
        report = valid_report()
        config = {**expected(), "workload": "canvas"}
        error = "canvas producer policy must disable acceleration and force-enable"
        for policy in [
            None,
            {},
            {"accelerated": True, "forceEnabled": False},
            {"accelerated": False, "forceEnabled": True},
            {"accelerated": 0, "forceEnabled": 0},
        ]:
            report["canvasPolicy"] = policy
            self.assertIn(error, validate_environment(report, config))
        report["canvasPolicy"] = {"accelerated": False, "forceEnabled": False}
        self.assertEqual(validate_environment(report, config), [])

    def test_environment_preflight_is_independent_of_workload(self):
        report = valid_report()
        for key in [
            "schemaVersion",
            "passed",
            "geometryAfter",
            "workload",
            "processMetrics",
            "diagnostics",
        ]:
            report.pop(key)
        self.assertEqual(validate_environment(report, expected()), [])
        report["backend"]["process"] = "Parent"
        self.assertIn("process mode mismatch", validate_environment(report, expected()))

    def test_valid_timing_report(self):
        self.assertEqual(validate_report(valid_report(), expected()), [])

    def test_backend_software_geometry_and_native_wsi_rejections(self):
        report = valid_report()
        report["backend"]["backend"] = "OpenGL"
        report["backend"]["renderer"] = "llvmpipe"
        report["presentation"]["wsiDebug"] = "sw"
        report["geometryAfter"]["viewport"] = [901, 700]
        errors = validate_report(report, expected())
        self.assertTrue(any("backend mismatch" in error for error in errors))
        self.assertIn("software renderer is not allowed", errors)
        self.assertIn("native presentation cannot use software WSI debug", errors)
        self.assertIn("geometryAfter.viewport mismatch", errors)

    def test_incomplete_workload_and_nonfinite_values(self):
        report = valid_report()
        report["workload"]["completed"] = False
        report["workload"]["durationMs"] = float("nan")
        report["workload"]["samples"] = [-1]
        errors = validate_report(report, expected())
        self.assertIn("workload.completed must be true", errors)
        self.assertIn("workload.durationMs must be finite and nonnegative", errors)
        self.assertIn(
            "workload.samples must contain finite nonnegative numbers", errors
        )

    def test_active_workload_duration_and_environment_events(self):
        report = valid_report()
        report["workload"].update(
            durationMs=5000,
            samples=[],
            updates=0,
            events=[{"type": "blur", "atMs": 100}],
        )
        errors = validate_report(report, expected())
        self.assertIn(
            "workload duration is outside the requested interval bound", errors
        )
        self.assertIn(
            "workload interval contains focus, resize, or visibility events", errors
        )
        self.assertIn("active workload must complete at least one update", errors)
        self.assertIn("active workload must record at least one frame interval", errors)

    def test_missing_host_provenance_and_pixel_oracle_are_rejected(self):
        report = valid_report()
        report["processMetrics"][0]["host"] = {}
        report["mappingsAfter"] = ["/snapshot/libxul.so"]
        report["libraryHashesBefore"] = {}
        report["pixels"]["checkpoint"] = None
        errors = validate_report(report, expected())
        self.assertIn("processMetrics[0].host.wholeHostCpuTicks is invalid", errors)
        self.assertIn("runtime library mappings changed", errors)
        self.assertIn("libraryHashesBefore is invalid", errors)
        self.assertIn("pixel checkpoint is missing", errors)

    def test_environment_errors_survive_full_report_validation(self):
        report = valid_report()
        report["backend"]["process"] = "Parent"
        invalid_expected = {**expected(), "phase": "unknown"}
        errors = validate_report(report, invalid_expected)
        self.assertIn("process mode mismatch", errors)
        self.assertIn("expected.phase is invalid", errors)

    def test_timing_requires_stable_processes(self):
        report = valid_report()
        report["processMetrics"][1]["processes"] = {
            "12:120": {
                **report["processMetrics"][1]["processes"]["10:100"],
                "pid": 12,
                "startTimeTicks": 120,
            }
        }
        self.assertIn(
            "timing process identities changed", validate_report(report, expected())
        )

    def test_memory_requires_stable_processes(self):
        report = valid_report()
        process = deepcopy(report["processMetrics"][1]["processes"]["10:100"])
        process.update(pid=12, startTimeTicks=120)
        report["processMetrics"][1]["processes"] = {"12:120": process}
        self.assertIn(
            "memory process identities changed",
            validate_report(report, expected("memory")),
        )

    def test_process_cpu_and_sample_clock_must_be_monotonic_and_bounded(self):
        report = valid_report()
        process = deepcopy(report["processMetrics"][1]["processes"]["10:100"])
        process.update(cpuSeconds=1.0, userCpuSeconds=0.7)
        report["processMetrics"][1]["processes"] = {"10:100": process}
        report["processMetrics"][1]["totals"] = {
            **report["processMetrics"][1]["totals"],
            "cpuSeconds": 1.0,
            "userCpuSeconds": 0.7,
        }
        report["processMetrics"][1]["timeSeconds"] = 2.2
        errors = validate_report(report, expected())
        self.assertIn("process 10:100 CPU counters decreased", errors)
        self.assertIn("total CPU counters decreased", errors)
        self.assertIn("processMetrics[1] lies outside the host interval", errors)

    def test_lifecycle_turnover_must_be_marked_lower_bound(self):
        report = valid_report()
        report["workload"]["name"] = "window-lifecycle"
        report["processMetrics"][1]["processes"] = {
            "12:120": {
                **report["processMetrics"][1]["processes"]["10:100"],
                "pid": 12,
                "startTimeTicks": 120,
            }
        }
        lifecycle = {**expected(), "workload": "window-lifecycle"}
        self.assertIn(
            "timing process identities changed", validate_report(report, lifecycle)
        )
        report["workload"]["events"] = [
            {"type": "process-turnover", "cpuAttribution": "lower-bound"}
        ]
        self.assertNotIn(
            "timing process identities changed", validate_report(report, lifecycle)
        )

    def test_diagnostics_only_for_vulkan_diagnostic_phase(self):
        report = valid_report()
        diagnostic_expected = expected("diagnostic")
        self.assertIn(
            "Vulkan diagnostic phase requires HAL diagnostics",
            validate_report(report, diagnostic_expected),
        )
        report["diagnostics"] = [diagnostic_record()]
        self.assertEqual(validate_report(report, diagnostic_expected), [])
        report["diagnostics"][0]["counters"].pop("executions")
        self.assertIn(
            "diagnostics[0].counters is incomplete",
            validate_report(report, diagnostic_expected),
        )
        report["diagnostics"] = [diagnostic_record()]
        self.assertIn(
            "HAL diagnostics are only allowed for Vulkan diagnostic phase",
            validate_report(report, expected("smoke")),
        )

    def test_gl_smoke_needs_no_hal_diagnostics_or_process_samples(self):
        report = valid_report()
        report["backend"].update(backend="OpenGL", renderer="Mesa Intel", driver="Mesa")
        report["presentation"] = {"mode": "xvfb", "wsiDebug": None}
        report["mappingsBefore"].append("/system/libGL.so.1")
        report["mappingsAfter"].append("/system/libGL.so.1")
        report["libraryHashesBefore"]["/system/libGL.so.1"] = "d" * 64
        report["libraryHashesAfter"]["/system/libGL.so.1"] = "d" * 64
        report["processMetrics"] = []
        report.pop("hostIntervalStart")
        report.pop("hostIntervalEnd")
        gl_expected = {
            **expected("smoke"),
            "backend": "gl",
            "renderer": "Mesa",
            "allowSoftware": True,
        }
        self.assertEqual(validate_report(report, gl_expected), [])

    def test_json_round_trip_preserves_schema(self):
        report = json.loads(json.dumps(valid_report()))
        self.assertEqual(validate_report(report, expected()), [])


if __name__ == "__main__":
    unittest.main()
