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
from unittest.mock import patch

import renderer_benchmark_metrics as metrics
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


def children_file(proc, pid, thread=None, children=()):
    path = proc / str(pid) / "task" / str(thread or pid) / "children"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(" ".join(str(child) for child in children))


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


def with_collector(report, available=True):
    report["processMetrics"] = deepcopy(report["processMetrics"])
    for index, sample in enumerate(report["processMetrics"]):
        sample["collector"] = {
            "available": available,
            "pid": 20 if available else None,
            "startTimeTicks": 200 if available else None,
            "identity": "20:200" if available else None,
            "userCpuSeconds": 0.1 + index * 0.01 if available else None,
            "systemCpuSeconds": 0.05 + index * 0.01 if available else None,
            "cpuSeconds": 0.15 + index * 0.02 if available else None,
        }
        sample["sampling"] = {
            "threadCpuSeconds": 0.003,
            "wallSeconds": 0.002,
        }
    return report


def with_timing_sampling(report):
    report["processMetrics"] = deepcopy(report["processMetrics"])
    first, last = report["processMetrics"]
    middle = deepcopy(first)
    middle["timeSeconds"] = 1.5
    for index, sample in enumerate([first, middle, last]):
        sample["detailLevel"] = "full" if index in [0, 2] else "light"
        sample["cpuSampleTimeSeconds"] = 1.0 + index * 0.5
        sample["processDiscovery"] = (
            {
                "method": "whole-proc-scan",
                "raceLimited": True,
                "retries": 0,
                "threadsVisited": None,
                "childLinks": None,
            }
            if index in [0, 2]
            else {
                "method": "proc-task-children",
                "raceLimited": True,
                "retries": 0,
                "threadsVisited": 1,
                "childLinks": 0,
            }
        )
    middle["drmClients"] = None
    for key in [
        "fdCount",
        "fdCoverage",
        "fdInfoCoverage",
        "pssBytes",
        "privateBytes",
        "memoryCoverage",
    ]:
        middle["totals"][key] = None
    for process in middle["processes"].values():
        process["fdCount"] = None
        process["fdInfoCoverage"] = None
        process["memory"] = None
    for key in [
        "cpuClocksKHz",
        "cpuGovernors",
        "temperaturesMilliC",
        "power",
        "platformProfile",
    ]:
        middle["host"][key] = None
    middle["host"]["backgroundProcessCpu"] = {
        "available": False,
        "processCount": None,
        "cpuSeconds": None,
        "reason": "not-collected",
    }
    report["processMetrics"] = [first, middle, last]
    return report


def profile_report():
    report = with_timing_sampling(with_collector(valid_report()))
    report["processMetrics"] = [deepcopy(sample) for sample in report["processMetrics"]]
    for sample in report["processMetrics"]:
        gpu = deepcopy(sample["processes"]["10:100"])
        gpu.update(pid=11, startTimeTicks=110)
        sample["processes"]["11:110"] = gpu
        for key in ["cpuSeconds", "userCpuSeconds", "systemCpuSeconds", "rssBytes"]:
            sample["totals"][key] += gpu[key]
        for total_key, process_key in [
            ("fdCount", "fdCount"),
            ("fdInfoCoverage", "fdInfoCoverage"),
        ]:
            if sample["totals"][total_key] is not None:
                sample["totals"][total_key] += gpu[process_key]
        if sample["totals"]["fdCoverage"] is not None:
            sample["totals"]["fdCoverage"] += 1
    report["processMetrics"][1]["processDiscovery"].update(
        threadsVisited=2, childLinks=1
    )
    report["samplingIntervalSeconds"] = 2
    report["processIds"] = {"parent": 10, "gpu": 11}
    report["startupSettling"] = {
        "passed": True,
        "minimumAgeSeconds": 65,
        "requiredStableSeconds": 5,
        "timeoutSeconds": 120,
        "elapsedSeconds": 70,
        "rootAgeSeconds": 75,
        "stableSeconds": 5,
        "rootIdentity": "10:100",
        "transitions": [
            {
                "elapsedSeconds": 0,
                "rootAgeSeconds": 5,
                "added": {"10:100": "firefox", "11:110": "GPU Process"},
                "removed": {},
            }
        ],
        "finalIdentities": ["10:100", "11:110"],
    }
    perf_binary = "/usr/lib/linux-hwe/perf"
    data_path = "/tmp/profile/perf.data"
    report["perf"] = {
        "passed": True,
        "binary": perf_binary,
        "binarySha256": "d" * 64,
        "event": "cpu-clock:uk",
        "eventAttributes": {
            "name": "cpu-clock:uk",
            "type": 1,
            "config": 0,
            "frequencyHz": 99,
            "frequencyMode": 1,
            "excludeUser": 0,
            "excludeKernel": 0,
            "excludeHypervisor": 1,
            "inherit": 1,
            "stackBytes": 16384,
            "clockId": 1,
            "sampleTypes": [
                "IP",
                "TID",
                "TIME",
                "CPU",
                "PERIOD",
                "REGS_USER",
                "STACK_USER",
            ],
        },
        "version": "perf version 6.17.13",
        "frequencyHz": 99,
        "callGraph": "dwarf,16384",
        "targetPid": 11,
        "targetIdentity": "11:110",
        "recorderPid": 30,
        "command": [
            perf_binary,
            "record",
            "-e",
            "cpu-clock:uk",
            "-F",
            "99",
            "--strict-freq",
            "--call-graph",
            "dwarf,16384",
            "-p",
            "11",
            "-D",
            "-1",
            "--control",
            "fifo:/tmp/profile/control,/tmp/profile/ack",
            "--clockid",
            "mono",
            "--timestamp",
            "--sample-cpu",
            "--no-buildid-cache",
            "-o",
            data_path,
        ],
        "controls": [
            {
                "command": "ping",
                "requestedTimeSeconds": 0.7,
                "acknowledgedTimeSeconds": 0.8,
            },
            {
                "command": "enable",
                "requestedTimeSeconds": 1.0,
                "acknowledgedTimeSeconds": 1.01,
            },
            {
                "command": "disable",
                "requestedTimeSeconds": 1.9,
                "acknowledgedTimeSeconds": 1.91,
            },
            {
                "command": "stop",
                "requestedTimeSeconds": 2.2,
                "acknowledgedTimeSeconds": 2.21,
            },
        ],
        "returncode": 0,
        "dataPath": data_path,
        "dataBytes": 4096,
        "dataSha256": "e" * 64,
    }
    config = {
        **expected("profile"),
        "collectorTelemetry": True,
        "timingSampling": True,
        "treeSampling": True,
        "startupSettling": True,
        "sampleIntervalSeconds": 2,
        "perf": {
            "binary": perf_binary,
            "sha256": "d" * 64,
            "event": "cpu-clock:uk",
            "version": "perf version 6.17.13",
        },
    }
    return report, config


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
            with patch("renderer_benchmark_metrics.os.getpid", return_value=20):
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
            self.assertEqual(sample["collector"]["identity"], "20:200")
            self.assertEqual(sample["collector"]["startTimeTicks"], 200)
            self.assertEqual(
                sample["collector"]["cpuSeconds"], 15 / os.sysconf("SC_CLK_TCK")
            )
            self.assertEqual(
                sample["totals"]["cpuSeconds"], 30 / os.sysconf("SC_CLK_TCK")
            )
            self.assertGreaterEqual(sample["sampling"]["threadCpuSeconds"], 0)
            self.assertGreaterEqual(sample["sampling"]["wallSeconds"], 0)
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
            with patch("renderer_benchmark_metrics.os.getpid", return_value=99):
                collector = sampler.sample_once()["collector"]
                self.assertFalse(collector["available"])
                self.assertTrue(
                    all(
                        collector[key] is None
                        for key in [
                            "pid",
                            "startTimeTicks",
                            "identity",
                            "userCpuSeconds",
                            "systemCpuSeconds",
                            "cpuSeconds",
                        ]
                    )
                )
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

    def test_light_sample_avoids_heavy_process_and_sysfs_reads(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            proc = root / "proc"
            sys = root / "sys"
            directory = proc / "10"
            directory.mkdir(parents=True)
            (directory / "stat").write_text(stat_line(10, 1, 100))
            (directory / "fdinfo").write_text("must not be read")
            children_file(proc, 10)
            (proc / "stat").write_text("cpu 1 2 3 4 5 6 7 8 9 10\n")
            (proc / "loadavg").write_text("0.10 0.20 0.30 2/50 123\n")
            sampler = Sampler(10, True, proc_root=proc, sys_root=sys)
            with patch(
                "renderer_benchmark_metrics._rollup",
                side_effect=AssertionError("smaps must not be read"),
            ):
                with patch(
                    "renderer_benchmark_metrics._optional_values",
                    side_effect=AssertionError("sysfs must not be read"),
                ):
                    sample = sampler.sample_once("light")
            self.assertEqual(sample["detailLevel"], "light")
            self.assertIsNone(sample["drmClients"])
            self.assertIsNone(sample["totals"]["fdCount"])
            self.assertIsNone(sample["host"]["cpuClocksKHz"])
            self.assertEqual(sample["processes"]["10:100"]["errors"], [])

    def test_timing_context_has_full_endpoints_and_light_intermediates(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            (proc / "10/fdinfo").mkdir(parents=True)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            children_file(proc, 10)
            (proc / "stat").write_text("cpu 1 2 3 4 5 6 7 8 9 10\n")
            (proc / "loadavg").write_text("0.10 0.20 0.30 2/50 123\n")
            with Sampler(
                10, timing=True, interval=0.01, proc_root=proc, sys_root=proc
            ) as sampler:
                time.sleep(0.025)
            levels = [sample["detailLevel"] for sample in sampler.samples]
            self.assertEqual(levels[0], "full")
            self.assertEqual(levels[-1], "full")
            self.assertTrue(levels[1:-1])
            self.assertEqual(set(levels[1:-1]), {"light"})

    def test_sample_detail_rejects_unknown_value(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            (proc / "10/stat").parent.mkdir(parents=True)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            with self.assertRaises(ValueError):
                sampler.sample_once("medium")

    def test_sampler_interval_must_be_finite_and_positive(self):
        for interval in [0, -1, float("nan"), float("inf"), True, "0.25"]:
            with self.subTest(interval=interval):
                with self.assertRaises(ValueError):
                    Sampler(10, interval=interval)
        self.assertEqual(Sampler(10, interval=0.01).interval, 0.01)
        self.assertEqual(Sampler(10, interval=10).interval, 10)

    def test_light_tree_finds_nonleader_and_recursive_children(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            for pid, parent, start in [
                (10, 1, 100),
                (20, 10, 200),
                (30, 20, 300),
            ]:
                path = proc / str(pid)
                path.mkdir(parents=True)
                (path / "stat").write_text(stat_line(pid, parent, start))
            children_file(proc, 10)
            children_file(proc, 10, thread=12, children=[20])
            children_file(proc, 20, children=[30])
            children_file(proc, 30)
            (proc / "stat").write_text("cpu 1 2 3 4 5 6 7 8 9 10\n")
            (proc / "loadavg").write_text("0.10 0.20 0.30 2/50 123\n")
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            with patch("renderer_benchmark_metrics.os.getpid", return_value=99):
                sample = sampler.sample_once("light")
            self.assertEqual(set(sample["processes"]), {"10:100", "20:200", "30:300"})
            self.assertEqual(
                sample["processDiscovery"],
                {
                    "method": "proc-task-children",
                    "raceLimited": True,
                    "retries": 0,
                    "threadsVisited": 4,
                    "childLinks": 2,
                },
            )

    def test_light_tree_rejects_missing_access_and_parent_mismatch(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            (proc / "10/stat").parent.mkdir(parents=True)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            (proc / "10/task/10/children").mkdir(parents=True)
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            with self.assertRaises(OSError):
                sampler.sample_once("light")

        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            for pid, parent, start in [(10, 1, 100), (20, 1, 200)]:
                path = proc / str(pid)
                path.mkdir(parents=True)
                (path / "stat").write_text(stat_line(pid, parent, start))
            children_file(proc, 10, children=[20])
            children_file(proc, 20)
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            with self.assertRaisesRegex(RuntimeError, "remained unstable"):
                sampler.sample_once("light")

        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            (proc / "10/stat").parent.mkdir(parents=True)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            children_file(proc, 10, children=[20])
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            with self.assertRaisesRegex(RuntimeError, "remained unstable"):
                sampler.sample_once("light")

        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            with self.assertRaises(ProcessLookupError):
                sampler.sample_once("light")

    def test_light_tree_detects_root_and_collector_reuse(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            for pid, parent, start in [(10, 1, 100), (20, 1, 200)]:
                path = proc / str(pid)
                path.mkdir(parents=True)
                (path / "stat").write_text(stat_line(pid, parent, start))
            children_file(proc, 10)
            (proc / "stat").write_text("cpu 1 2 3 4 5 6 7 8 9 10\n")
            (proc / "loadavg").write_text("0.10 0.20 0.30 2/50 123\n")
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            sampler.sample_once("light")
            (proc / "10/stat").write_text(stat_line(10, 1, 101))
            with self.assertRaisesRegex(RuntimeError, "remained unstable"):
                sampler.sample_once("light")

            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            (proc / "10/stat").write_text(stat_line(10, 1, 100))
            original = metrics._read_stat
            collector_reads = 0

            def changing_collector(path):
                nonlocal collector_reads
                stat = original(path)
                if path == proc / "20/stat":
                    collector_reads += 1
                    stat["startTimeTicks"] += collector_reads % 2
                return stat

            with patch("renderer_benchmark_metrics.os.getpid", return_value=20):
                with patch(
                    "renderer_benchmark_metrics._read_stat",
                    side_effect=changing_collector,
                ):
                    with self.assertRaisesRegex(RuntimeError, "remained unstable"):
                        sampler.sample_once("light")

    def test_light_tree_reports_bounded_race_retry(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            for pid, parent, start in [(10, 1, 100), (20, 10, 200)]:
                path = proc / str(pid)
                path.mkdir(parents=True)
                (path / "stat").write_text(stat_line(pid, parent, start))
            children_file(proc, 10, children=[20])
            children_file(proc, 20)
            (proc / "stat").write_text("cpu 1 2 3 4 5 6 7 8 9 10\n")
            (proc / "loadavg").write_text("0.10 0.20 0.30 2/50 123\n")
            sampler = Sampler(10, proc_root=proc, sys_root=proc)
            original = metrics._read_stat
            missing_once = True

            def transient_child(path):
                nonlocal missing_once
                if path == proc / "20/stat" and missing_once:
                    missing_once = False
                    raise FileNotFoundError(path)
                return original(path)

            with patch("renderer_benchmark_metrics.os.getpid", return_value=99):
                with patch(
                    "renderer_benchmark_metrics._read_stat",
                    side_effect=transient_child,
                ):
                    sample = sampler.sample_once("light")
            self.assertEqual(sample["processDiscovery"]["retries"], 1)
            self.assertEqual(set(sample["processes"]), {"10:100", "20:200"})


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

    def test_required_collector_telemetry_is_validated(self):
        required = {**expected(), "collectorTelemetry": True}
        self.assertIn(
            "processMetrics[0].collector is missing",
            validate_report(valid_report(), required),
        )
        report = with_collector(valid_report())
        self.assertEqual(validate_report(report, required), [])

    def test_collector_identity_cpu_and_sampling_rejections(self):
        required = {**expected(), "collectorTelemetry": True}

        report = with_collector(valid_report())
        report["processMetrics"][0]["collector"]["identity"] = "20:201"
        self.assertIn(
            "processMetrics[0].collector identity is invalid",
            validate_report(report, required),
        )

        report = with_collector(valid_report())
        report["processMetrics"][0]["collector"]["cpuSeconds"] = 9
        self.assertIn(
            "processMetrics[0].collector CPU is invalid",
            validate_report(report, required),
        )

        report = with_collector(valid_report())
        report["processMetrics"][0]["sampling"]["threadCpuSeconds"] = float("nan")
        self.assertIn(
            "processMetrics[0].sampling is invalid",
            validate_report(report, required),
        )

    def test_collector_must_be_stable_monotonic_and_outside_target_tree(self):
        required = {**expected(), "collectorTelemetry": True}

        report = with_collector(valid_report())
        report["processMetrics"][1]["collector"].update(
            pid=21, startTimeTicks=210, identity="21:210"
        )
        self.assertIn("collector identity changed", validate_report(report, required))

        report = with_collector(valid_report())
        report["processMetrics"][1]["collector"].update(
            userCpuSeconds=0.09, systemCpuSeconds=0.04, cpuSeconds=0.13
        )
        self.assertIn(
            "collector CPU counters decreased", validate_report(report, required)
        )

        report = with_collector(valid_report())
        report["processMetrics"][0]["collector"].update(
            pid=10, startTimeTicks=100, identity="10:100"
        )
        self.assertIn(
            "processMetrics[0].collector overlaps the target tree",
            validate_report(report, required),
        )

    def test_unavailable_collector_is_legacy_only_and_nullable(self):
        report = with_collector(valid_report(), available=False)
        self.assertEqual(validate_report(report, expected()), [])
        required = {**expected(), "collectorTelemetry": True}
        self.assertIn(
            "processMetrics[0].collector is unavailable",
            validate_report(report, required),
        )
        report["processMetrics"][0]["collector"]["pid"] = 20
        self.assertIn(
            "processMetrics[0].unavailable collector fields must be null",
            validate_report(report, expected()),
        )
        report = with_collector(valid_report(), available=False)
        report["processMetrics"][0]["collector"].pop("startTimeTicks")
        self.assertIn(
            "processMetrics[0].unavailable collector fields must be null",
            validate_report(report, expected()),
        )

    def test_timing_sampling_requires_full_endpoints_and_light_middle(self):
        report = with_timing_sampling(valid_report())
        config = {**expected(), "timingSampling": True, "treeSampling": True}
        self.assertEqual(validate_report(report, config), [])

        report = with_timing_sampling(valid_report())
        report["processMetrics"][0]["detailLevel"] = "light"
        self.assertIn(
            "timing sampling requires full endpoint samples",
            validate_report(report, config),
        )

        report = with_timing_sampling(valid_report())
        report["processMetrics"][1]["detailLevel"] = "medium"
        errors = validate_report(report, config)
        self.assertIn("timing sampling requires light intermediate samples", errors)
        self.assertIn("processMetrics[1].detailLevel is invalid", errors)

    def test_prior_tiered_report_without_tree_schema_remains_valid(self):
        report = with_timing_sampling(valid_report())
        for sample in report["processMetrics"]:
            sample.pop("processDiscovery")
        report["processMetrics"][1]["host"]["backgroundProcessCpu"] = {
            "available": True,
            "processCount": 1,
            "cpuSeconds": 2.0,
            "reason": None,
        }
        config = {**expected(), "timingSampling": True}
        self.assertEqual(validate_report(report, config), [])

    def test_light_sample_rejects_collected_heavy_data(self):
        report = with_timing_sampling(valid_report())
        config = {**expected(), "timingSampling": True, "treeSampling": True}
        middle = report["processMetrics"][1]
        middle["totals"]["fdCount"] = 1
        middle["drmClients"] = {}
        middle["host"]["cpuClocksKHz"] = {
            "available": False,
            "values": {},
            "errors": {},
            "reason": "not-available",
        }
        middle["processes"]["10:100"]["fdCount"] = 1
        middle["host"]["backgroundProcessCpu"] = {
            "available": True,
            "processCount": 1,
            "cpuSeconds": 2,
            "reason": None,
        }
        errors = validate_report(report, config)
        self.assertIn("processMetrics[1].light totals must omit heavy data", errors)
        self.assertIn("processMetrics[1].light drmClients must be null", errors)
        self.assertIn("processMetrics[1].light host.cpuClocksKHz must be null", errors)
        self.assertIn("processMetrics[1].host.backgroundProcessCpu is invalid", errors)
        self.assertIn("process 10:100 light heavy data must be null", errors)

    def test_timing_sampling_rejects_missing_or_malformed_discovery(self):
        config = {**expected(), "timingSampling": True, "treeSampling": True}
        report = with_timing_sampling(valid_report())
        report["processMetrics"][1].pop("processDiscovery")
        self.assertIn(
            "processMetrics[1].processDiscovery is invalid",
            validate_report(report, config),
        )

        report = with_timing_sampling(valid_report())
        report["processMetrics"][1]["processDiscovery"]["threadsVisited"] = 0
        self.assertIn(
            "processMetrics[1].light processDiscovery is invalid",
            validate_report(report, config),
        )

    def test_timing_sampling_requires_ordered_cpu_anchor_span(self):
        config = {**expected(), "timingSampling": True, "treeSampling": True}
        report = with_timing_sampling(valid_report())
        report["processMetrics"][-1]["cpuSampleTimeSeconds"] = 1.0
        self.assertIn(
            "timing sampling requires a positive endpoint interval",
            validate_report(report, config),
        )

        report = with_timing_sampling(valid_report())
        report["processMetrics"][0].pop("cpuSampleTimeSeconds")
        self.assertIn(
            "processMetrics[0].cpuSampleTimeSeconds is invalid",
            validate_report(report, config),
        )

        report = with_timing_sampling(valid_report())
        report["processMetrics"][1]["cpuSampleTimeSeconds"] = 1.6
        self.assertIn(
            "processMetrics[1] CPU sample time exceeds sample time",
            validate_report(report, config),
        )

    def test_report_sample_interval_contract(self):
        report = valid_report()
        report["samplingIntervalSeconds"] = 0.25
        config = {**expected(), "sampleIntervalSeconds": 0.25}
        self.assertEqual(validate_report(report, config), [])

        report.pop("samplingIntervalSeconds")
        self.assertIn(
            "report sample interval is missing", validate_report(report, config)
        )

        report["samplingIntervalSeconds"] = 2
        self.assertIn(
            "report sample interval mismatch", validate_report(report, config)
        )

        report["samplingIntervalSeconds"] = float("nan")
        self.assertIn(
            "report sample interval is invalid", validate_report(report, config)
        )

        for interval in [0.2, 2.1, float("nan"), float("inf"), True]:
            with self.subTest(interval=interval):
                invalid = {**expected(), "sampleIntervalSeconds": interval}
                self.assertIn(
                    "expected sample interval is invalid",
                    validate_report(valid_report(), invalid),
                )

        legacy = valid_report()
        legacy["samplingIntervalSeconds"] = 10
        self.assertEqual(validate_report(legacy, expected()), [])

    def test_valid_profile_report(self):
        report, config = profile_report()
        self.assertEqual(validate_report(report, config), [])

        report, config = profile_report()
        report["perf"]["event"] = "cpu-clock:u"
        report["perf"]["command"][report["perf"]["command"].index("cpu-clock:uk")] = (
            "cpu-clock:u"
        )
        report["perf"]["eventAttributes"].update(name="cpu-clock:u", excludeKernel=1)
        config["perf"]["event"] = "cpu-clock:u"
        self.assertEqual(validate_report(report, config), [])

    def test_profile_metadata_is_required_only_for_profile_phase(self):
        report, config = profile_report()
        report.pop("perf")
        self.assertIn(
            "profile phase requires perf metadata", validate_report(report, config)
        )

        report, config = profile_report()
        config["phase"] = "timing"
        self.assertIn(
            "expected perf configuration is only allowed for profile phase",
            validate_report(report, config),
        )
        self.assertIn(
            "perf metadata is only allowed for profile phase",
            validate_report(report, config),
        )

        report, config = profile_report()
        config["sampleIntervalSeconds"] = 0.25
        self.assertIn(
            "profile phase requires settled native GPU timing sampling",
            validate_report(report, config),
        )

    def test_profile_target_and_recorder_identity_are_validated(self):
        report, config = profile_report()
        report["perf"]["targetPid"] = 12
        self.assertIn(
            "perf target does not match the sampled GPU process",
            validate_report(report, config),
        )

        report, config = profile_report()
        report["perf"]["targetIdentity"] = "11:111"
        self.assertIn(
            "perf target does not match the sampled GPU process",
            validate_report(report, config),
        )

        report, config = profile_report()
        report["perf"]["recorderPid"] = 10
        self.assertIn(
            "perf recorder overlaps the Firefox process tree",
            validate_report(report, config),
        )

    def test_profile_capture_metadata_is_validated(self):
        mutations = [
            ("binarySha256", "bad"),
            ("event", "cycles"),
            ("version", "wrong"),
            ("frequencyHz", 100),
            ("callGraph", "fp"),
            ("returncode", 1),
            ("dataBytes", 0),
            ("dataSha256", "bad"),
            ("passed", False),
        ]
        for key, value in mutations:
            with self.subTest(key=key):
                report, config = profile_report()
                report["perf"][key] = value
                self.assertIn(
                    "perf capture metadata is invalid",
                    validate_report(report, config),
                )

        report, config = profile_report()
        report["perf"]["command"].remove("--strict-freq")
        self.assertIn(
            "perf capture metadata is invalid", validate_report(report, config)
        )

        for forbidden in ["-a", "--all-cpus", "-t", "--tid"]:
            with self.subTest(forbidden=forbidden):
                report, config = profile_report()
                report["perf"]["command"].append(forbidden)
                self.assertIn(
                    "perf capture metadata is invalid",
                    validate_report(report, config),
                )

        report, config = profile_report()
        config["perf"]["sha256"] = "bad"
        self.assertIn(
            "expected perf configuration is invalid", validate_report(report, config)
        )

    def test_profile_actual_event_attributes_are_validated(self):
        report, config = profile_report()
        report["perf"].pop("eventAttributes")
        self.assertIn(
            "perf event attributes are invalid", validate_report(report, config)
        )

        mutations = [
            ("name", "cpu-clock:u"),
            ("type", 0),
            ("config", 1),
            ("frequencyHz", 100),
            ("frequencyMode", 0),
            ("excludeUser", 1),
            ("excludeKernel", 1),
            ("inherit", 0),
            ("stackBytes", 8192),
            ("clockId", 0),
        ]
        for key, value in mutations:
            with self.subTest(key=key):
                report, config = profile_report()
                report["perf"]["eventAttributes"][key] = value
                self.assertIn(
                    "perf event attributes are invalid",
                    validate_report(report, config),
                )

        report, config = profile_report()
        report["perf"]["eventAttributes"]["sampleTypes"].remove("STACK_USER")
        self.assertIn(
            "perf event attributes are invalid", validate_report(report, config)
        )

    def test_profile_control_acknowledgements_and_boundaries_are_validated(self):
        report, config = profile_report()
        report["perf"]["controls"][1]["command"] = "disable"
        self.assertIn(
            "perf control acknowledgement 1 is invalid",
            validate_report(report, config),
        )

        report, config = profile_report()
        report["perf"]["controls"][2]["acknowledgedTimeSeconds"] = float("nan")
        self.assertIn(
            "perf control acknowledgement 2 is invalid",
            validate_report(report, config),
        )

        for index, key, value in [
            (0, "acknowledgedTimeSeconds", 1.0),
            (1, "requestedTimeSeconds", 0.8),
            (2, "acknowledgedTimeSeconds", 2.2),
            (3, "requestedTimeSeconds", 2.0),
        ]:
            with self.subTest(index=index, key=key):
                report, config = profile_report()
                report["perf"]["controls"][index][key] = value
                self.assertIn(
                    "perf control boundaries are invalid",
                    validate_report(report, config),
                )

    def test_profile_validation_preserves_errors_for_malformed_process_samples(self):
        report, config = profile_report()
        report["processMetrics"][1]["processes"] = []
        errors = validate_report(report, config)
        self.assertIn("processMetrics[1].processes is missing", errors)

        report, config = profile_report()
        report["processMetrics"][1]["processes"]["11:110"]["pid"] = []
        errors = validate_report(report, config)
        self.assertIn("process 11:110 identity is invalid", errors)

    def test_startup_settling_gate(self):
        report = valid_report()
        report["startupSettling"] = {
            "passed": True,
            "minimumAgeSeconds": 65,
            "requiredStableSeconds": 5,
            "timeoutSeconds": 120,
            "elapsedSeconds": 66,
            "rootAgeSeconds": 70,
            "stableSeconds": 5,
            "rootIdentity": "10:100",
            "transitions": [
                {
                    "elapsedSeconds": 0.1,
                    "rootAgeSeconds": 65,
                    "added": {"10:100": "firefox"},
                    "removed": {},
                }
            ],
            "finalIdentities": ["10:100", "11:110"],
        }
        config = {**expected(), "startupSettling": True}
        self.assertEqual(validate_report(report, config), [])

        report["startupSettling"]["rootIdentity"] = "11:110"
        self.assertIn(
            "startup settling root identity mismatch",
            validate_report(report, config),
        )
        report["startupSettling"].update(rootIdentity="10:100", elapsedSeconds=120)
        self.assertIn(
            "startup settling report is invalid", validate_report(report, config)
        )
        report["startupSettling"].update(elapsedSeconds="bad", stableSeconds=4)
        self.assertIn(
            "startup settling report is invalid", validate_report(report, config)
        )

    def test_startup_settling_transition_and_final_identity_rejections(self):
        report = valid_report()
        report["startupSettling"] = {
            "passed": True,
            "minimumAgeSeconds": 65,
            "requiredStableSeconds": 5,
            "timeoutSeconds": 120,
            "elapsedSeconds": 66,
            "rootAgeSeconds": 70,
            "stableSeconds": 5,
            "rootIdentity": "10:100",
            "transitions": [],
            "finalIdentities": ["10:100"],
        }
        config = {**expected(), "startupSettling": True}
        self.assertIn(
            "startup settling transitions are invalid",
            validate_report(report, config),
        )

        report["startupSettling"]["transitions"] = [
            {
                "elapsedSeconds": 67,
                "rootAgeSeconds": -1,
                "added": {10: "firefox"},
                "removed": [],
            }
        ]
        self.assertIn(
            "startup settling transition 0 is invalid",
            validate_report(report, config),
        )
        report["startupSettling"]["transitions"] = [
            {
                "elapsedSeconds": 1,
                "rootAgeSeconds": 66,
                "added": {"10:100": "firefox"},
                "removed": {},
            },
            {
                "elapsedSeconds": 0.5,
                "rootAgeSeconds": 66.5,
                "added": {},
                "removed": {},
            },
        ]
        self.assertIn(
            "startup settling transition 1 is invalid",
            validate_report(report, config),
        )
        report["startupSettling"]["finalIdentities"] = ["11:110", "11:110"]
        self.assertIn(
            "startup settling final identities are invalid",
            validate_report(report, config),
        )

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
