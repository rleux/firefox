# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import math
import os
import threading
import time
from pathlib import Path

HAL_ENVELOPE = {
    "version",
    "pid",
    "deviceId",
    "rendererId",
    "sequence",
    "monotonicNs",
    "final",
    "counters",
    "gauges",
    "peaks",
    "lastWorkNs",
}
SOFTWARE_RENDERERS = ["llvmpipe", "lavapipe", "softpipe", "software", "swiftshader"]
HAL_COUNTERS = {
    "frameReady",
    "renderRequested",
    "noRenderRequested",
    "scrolledRequests",
    "forceRedraws",
    "wakeRender",
    "wakeUpdate",
    "updates",
    "executions",
    "offscreenExecutions",
    "rasterizedTiles",
    "fullCompositions",
    "partialCompositions",
    "composedPixels",
    "acquires",
    "presents",
    "fullPresentUpdates",
    "partialPresentUpdates",
    "unchangedPresentUpdates",
    "presentPixels",
    "discards",
    "queueSubmissions",
    "surfaceSubmissions",
    "completedSubmissions",
    "polls",
    "externalLeaseAcquires",
    "externalLeaseReleases",
    "resourceUploads",
    "resourceUploadBytes",
    "readbacks",
    "reusedOutputs",
    "hiddenSkips",
}
HAL_GAUGES = {
    "pendingSubmissions",
    "externalLeases",
    "retainedOutputBytes",
    "initializedSurfaceImages",
    "textureBytes",
    "bufferBytes",
}


def _finite_number(value):
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
    )


def _sha256(value):
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value.lower())
    )


def _read_stat(path):
    text = path.read_text()
    close = text.rfind(")")
    if close < 0:
        raise ValueError(f"Malformed proc stat: {path}")
    pid = int(text[: text.index(" ")])
    comm = text[text.index("(") + 1 : close]
    fields = text[close + 2 :].split()
    if len(fields) < 22:
        raise ValueError(f"Truncated proc stat: {path}")
    return {
        "pid": pid,
        "comm": comm,
        "parentPid": int(fields[1]),
        "userTicks": int(fields[11]),
        "systemTicks": int(fields[12]),
        "startTimeTicks": int(fields[19]),
        "rssPages": int(fields[21]),
    }


def _rollup(path):
    values = {}
    for line in path.read_text().splitlines():
        if ":" not in line:
            continue
        name, value = line.split(":", 1)
        fields = value.split()
        if fields:
            values[name] = int(fields[0]) * 1024
    return {
        "pssBytes": values["Pss"],
        "privateBytes": values.get("Private_Clean", 0) + values.get("Private_Dirty", 0),
    }


def _optional_values(paths, convert):
    result = {}
    errors = {}
    for path in paths:
        try:
            result[str(path)] = convert(path.read_text().strip())
        except OSError as error:
            errors[str(path)] = str(error)
        except ValueError as error:
            errors[str(path)] = str(error)
    return {
        "available": bool(result),
        "values": result,
        "errors": errors,
        "reason": None if result else "not-available",
    }


def _optional_value(path, convert):
    try:
        return {
            "available": True,
            "value": convert(path.read_text().strip()),
            "error": None,
            "reason": None,
        }
    except (OSError, ValueError) as error:
        return {
            "available": False,
            "value": None,
            "error": str(error),
            "reason": "not-available",
        }


class Sampler:
    def __init__(
        self,
        root_pid,
        include_memory=False,
        interval=0.25,
        proc_root=Path("/proc"),
        sys_root=Path("/sys"),
        clock=time.monotonic,
    ):
        if interval <= 0:
            raise ValueError("interval must be positive")
        self.root_pid = int(root_pid)
        self.include_memory = include_memory
        self.interval = float(interval)
        self.proc_root = Path(proc_root)
        self.sys_root = Path(sys_root)
        self.clock = clock
        self.samples = []
        self._root_identity = None
        self._stop = threading.Event()
        self._thread = None
        self._error = None

    def _stats(self):
        result = {}
        for entry in self.proc_root.iterdir():
            if not entry.name.isdigit():
                continue
            try:
                stat = _read_stat(entry / "stat")
            except FileNotFoundError:
                continue
            result[stat["pid"]] = stat
        root = result.get(self.root_pid)
        if root is None:
            raise ProcessLookupError(f"Root process {self.root_pid} is unavailable")
        identity = f"{self.root_pid}:{root['startTimeTicks']}"
        if self._root_identity is None:
            self._root_identity = identity
        elif self._root_identity != identity:
            raise RuntimeError("Root process identity changed")
        owned = {self.root_pid}
        while True:
            expanded = owned | {
                pid for pid, stat in result.items() if stat["parentPid"] in owned
            }
            if expanded == owned:
                return result, owned
            owned = expanded

    def _host(self, stats, owned, tick):
        clocks = _optional_values(
            self.sys_root.glob("devices/system/cpu/cpu[0-9]*/cpufreq/scaling_cur_freq"),
            int,
        )
        temperatures = _optional_values(
            self.sys_root.glob("class/thermal/thermal_zone*/temp"), int
        )
        power_paths = list(self.sys_root.glob("class/power_supply/*/online"))
        power_paths += list(self.sys_root.glob("class/power_supply/*/status"))
        power_paths += list(self.sys_root.glob("class/power_supply/*/capacity"))
        power = _optional_values(power_paths, str)
        governors = _optional_values(
            self.sys_root.glob("devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor"),
            str,
        )
        platform_profile = _optional_value(
            self.sys_root / "firmware/acpi/platform_profile", str
        )

        def cpu_stat(text):
            line = next(
                (line for line in text.splitlines() if line.startswith("cpu ")), None
            )
            if line is None:
                raise ValueError("aggregate CPU line is missing")
            names = [
                "user",
                "nice",
                "system",
                "idle",
                "iowait",
                "irq",
                "softirq",
                "steal",
                "guest",
                "guestNice",
            ]
            return {name: int(value) for name, value in zip(names, line.split()[1:])}

        def load_average(text):
            fields = text.split()
            if len(fields) < 4 or "/" not in fields[3]:
                raise ValueError("load average is malformed")
            runnable, processes = fields[3].split("/", 1)
            return {
                "oneMinute": float(fields[0]),
                "fiveMinutes": float(fields[1]),
                "fifteenMinutes": float(fields[2]),
                "runnable": int(runnable),
                "processes": int(processes),
            }

        background = [stat for pid, stat in stats.items() if pid not in owned]
        return {
            "wholeHostCpuTicks": _optional_value(self.proc_root / "stat", cpu_stat),
            "loadAverage": _optional_value(self.proc_root / "loadavg", load_average),
            "backgroundProcessCpu": {
                "available": True,
                "processCount": len(background),
                "cpuSeconds": sum(
                    stat["userTicks"] + stat["systemTicks"] for stat in background
                )
                / tick,
                "reason": None,
            },
            "cpuClocksKHz": clocks,
            "cpuGovernors": governors,
            "temperaturesMilliC": temperatures,
            "power": power,
            "platformProfile": platform_profile,
        }

    def sample_once(self):
        sampling_start = self.clock()
        sampling_cpu_start = time.thread_time()
        stats, owned = self._stats()
        tick = os.sysconf("SC_CLK_TCK")
        page = os.sysconf("SC_PAGE_SIZE")
        processes = {}
        drm_clients = {}
        for pid in sorted(owned):
            stat = stats[pid]
            identity = f"{pid}:{stat['startTimeTicks']}"
            process = {
                "pid": pid,
                "startTimeTicks": stat["startTimeTicks"],
                "parentPid": stat["parentPid"],
                "comm": stat["comm"],
                "userCpuSeconds": stat["userTicks"] / tick,
                "systemCpuSeconds": stat["systemTicks"] / tick,
                "cpuSeconds": (stat["userTicks"] + stat["systemTicks"]) / tick,
                "rssBytes": stat["rssPages"] * page,
                "fdCount": None,
                "fdInfoCoverage": 0,
                "memory": None,
                "errors": [],
            }
            if self.include_memory:
                try:
                    process["memory"] = _rollup(
                        self.proc_root / str(pid) / "smaps_rollup"
                    )
                except OSError as error:
                    process["errors"].append(f"smaps_rollup: {error}")
                except (KeyError, ValueError) as error:
                    raise ValueError(f"Invalid smaps_rollup for {identity}: {error}")
            fdinfo = self.proc_root / str(pid) / "fdinfo"
            try:
                descriptors = list(fdinfo.iterdir())
                process["fdCount"] = len(descriptors)
            except FileNotFoundError:
                process["errors"].append("fdinfo: process departed")
                descriptors = []
            except OSError as error:
                process["errors"].append(f"fdinfo: {error}")
                descriptors = []
            for descriptor in descriptors:
                try:
                    lines = descriptor.read_text().splitlines()
                    process["fdInfoCoverage"] += 1
                    raw = {
                        name: value.strip()
                        for line in lines
                        if line.startswith("drm-")
                        for name, value in [line.split(":", 1)]
                    }
                except FileNotFoundError:
                    continue
                except OSError as error:
                    process["errors"].append(f"fdinfo/{descriptor.name}: {error}")
                    continue
                if "drm-client-id" not in raw:
                    continue
                drm_identity = (
                    raw.get("drm-pdev", "global") + "/" + raw["drm-client-id"]
                )
                client = drm_clients.setdefault(
                    drm_identity,
                    {
                        "identity": drm_identity,
                        "raw": raw,
                        "firstRaw": raw,
                        "lastRaw": raw,
                        "observations": 0,
                        "owners": [],
                    },
                )
                client["raw"] = raw
                client["lastRaw"] = raw
                client["observations"] += 1
                if identity not in client["owners"]:
                    client["owners"].append(identity)
            processes[identity] = process
        totals = {
            "cpuSeconds": sum(value["cpuSeconds"] for value in processes.values()),
            "userCpuSeconds": sum(
                value["userCpuSeconds"] for value in processes.values()
            ),
            "systemCpuSeconds": sum(
                value["systemCpuSeconds"] for value in processes.values()
            ),
            "rssBytes": sum(value["rssBytes"] for value in processes.values()),
            "fdCount": sum(value["fdCount"] or 0 for value in processes.values()),
            "fdCoverage": sum(
                value["fdCount"] is not None for value in processes.values()
            ),
            "fdInfoCoverage": sum(
                value["fdInfoCoverage"] for value in processes.values()
            ),
            "pssBytes": sum(
                value["memory"]["pssBytes"]
                for value in processes.values()
                if value["memory"] is not None
            ),
            "privateBytes": sum(
                value["memory"]["privateBytes"]
                for value in processes.values()
                if value["memory"] is not None
            ),
            "memoryCoverage": sum(
                value["memory"] is not None for value in processes.values()
            ),
        }
        result = {
            "timeSeconds": self.clock(),
            "rootIdentity": self._root_identity,
            "processes": processes,
            "totals": totals,
            "drmClients": drm_clients,
            "host": self._host(stats, owned, tick),
        }
        collector_pid = os.getpid()
        collector = stats.get(collector_pid)
        result["collector"] = {
            "available": collector is not None,
            "pid": collector_pid if collector else None,
            "startTimeTicks": collector["startTimeTicks"] if collector else None,
            "identity": f"{collector_pid}:{collector['startTimeTicks']}"
            if collector
            else None,
            "userCpuSeconds": collector["userTicks"] / tick if collector else None,
            "systemCpuSeconds": collector["systemTicks"] / tick if collector else None,
            "cpuSeconds": (collector["userTicks"] + collector["systemTicks"]) / tick
            if collector
            else None,
        }
        result["sampling"] = {
            "threadCpuSeconds": time.thread_time() - sampling_cpu_start,
            "wallSeconds": self.clock() - sampling_start,
        }
        return result

    def _run(self):
        try:
            while not self._stop.wait(self.interval):
                self.samples.append(self.sample_once())
        except BaseException as error:
            self._error = error
            self._stop.set()

    def __enter__(self):
        self.samples.append(self.sample_once())
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, exception_type, *_):
        self._stop.set()
        self._thread.join()
        final_error = None
        try:
            self.samples.append(self.sample_once())
        except BaseException as error:
            final_error = error
        if exception_type is None:
            if self._error is not None:
                raise self._error
            if final_error is not None:
                raise final_error


def validate_environment(report, expected):
    errors = []
    if not isinstance(report, dict):
        return ["report must be an object"]
    if expected.get("workload") == "canvas":
        policy = report.get("canvasPolicy")
        if (
            not isinstance(policy, dict)
            or policy.get("accelerated") is not False
            or policy.get("forceEnabled") is not False
        ):
            errors.append(
                "canvas producer policy must disable acceleration and force-enable"
            )
    backend = report.get("backend")
    if not isinstance(backend, dict):
        return ["missing report.backend"]
    for key in ["backend", "renderer", "driver", "process"]:
        if key not in backend:
            errors.append(f"missing backend.{key}")
    backend_names = {
        "gl": ["OpenGL", "OpenGL ES"],
        "vulkan": ["Vulkan (wgpu-hal)"],
    }
    expected_backend = expected.get("backend")
    if expected_backend not in backend_names:
        errors.append("expected.backend must be gl or vulkan")
    elif backend.get("backend") not in backend_names[expected_backend]:
        errors.append(
            f"backend mismatch: expected one of {backend_names[expected_backend]!r}, got {backend.get('backend')!r}"
        )
    renderer = backend.get("renderer")
    renderer_match = expected.get("renderer")
    if renderer_match and (
        not isinstance(renderer, str) or renderer_match not in renderer
    ):
        errors.append("renderer identity mismatch")
    if backend.get("process") != expected.get("process"):
        errors.append("process mode mismatch")
    if not expected.get("allowSoftware", False) and isinstance(renderer, str):
        lowered = renderer.lower()
        if any(name in lowered for name in SOFTWARE_RENDERERS):
            errors.append("software renderer is not allowed")

    presentation = report.get("presentation")
    if not isinstance(presentation, dict):
        errors.append("missing report.presentation")
    else:
        for key in ["mode", "wsiDebug"]:
            if key not in presentation:
                errors.append(f"missing presentation.{key}")
        mode = presentation.get("mode")
        if mode not in ["native", "xvfb"]:
            errors.append("presentation.mode must be native or xvfb")
        if mode == "native" and presentation.get("wsiDebug") is not None:
            errors.append("native presentation cannot use software WSI debug")
        if expected.get("phase") == "timing" and mode != "native":
            errors.append("timing phase requires native presentation")
    if expected.get("phase") not in ["smoke", "timing", "diagnostic", "memory"]:
        errors.append("expected.phase is invalid")

    geometry = report.get("geometryBefore")
    if not isinstance(geometry, dict):
        errors.append("missing report.geometryBefore")
    else:
        if geometry.get("viewport") != expected.get("viewport"):
            errors.append("geometryBefore.viewport mismatch")
        dpr = geometry.get("dpr")
        if not _finite_number(dpr) or dpr != expected.get("dpr"):
            errors.append("geometryBefore.dpr mismatch")
        if geometry.get("visible") is not True:
            errors.append("geometryBefore.visible must be true")
        if geometry.get("focused") is not True:
            errors.append("geometryBefore.focused must be true")
    return errors


def validate_report(report, expected):
    errors = validate_environment(report, expected)

    def require(mapping, key, label):
        if not isinstance(mapping, dict) or key not in mapping:
            errors.append(f"missing {label}.{key}")
            return None
        return mapping[key]

    if not isinstance(report, dict):
        return errors
    if report.get("schemaVersion") != 1:
        errors.append("schemaVersion must be 1")
    if report.get("passed") is not True:
        errors.append("passed must be true")
    expected_backend = expected.get("backend")
    geometry = require(report, "geometryAfter", "report")
    viewport = require(geometry, "viewport", "geometryAfter")
    dpr = require(geometry, "dpr", "geometryAfter")
    visible = require(geometry, "visible", "geometryAfter")
    focused = require(geometry, "focused", "geometryAfter")
    if viewport != expected.get("viewport"):
        errors.append("geometryAfter.viewport mismatch")
    if not _finite_number(dpr) or dpr != expected.get("dpr"):
        errors.append("geometryAfter.dpr mismatch")
    if visible is not True:
        errors.append("geometryAfter.visible must be true")
    if focused is not True:
        errors.append("geometryAfter.focused must be true")

    workload = require(report, "workload", "report")
    if require(workload, "name", "workload") != expected.get("workload"):
        errors.append("workload name mismatch")
    for key in ["requestedDurationMs", "durationMs"]:
        value = require(workload, key, "workload")
        if not _finite_number(value) or value < 0:
            errors.append(f"workload.{key} must be finite and nonnegative")
    requested = (
        workload.get("requestedDurationMs") if isinstance(workload, dict) else None
    )
    duration = workload.get("durationMs") if isinstance(workload, dict) else None
    if _finite_number(requested) and _finite_number(duration):
        upper_slack = max(1000, requested * 0.1)
        if duration < requested * 0.95 or duration > requested + upper_slack:
            errors.append("workload duration is outside the requested interval bound")
    if require(workload, "completed", "workload") is not True:
        errors.append("workload.completed must be true")
    samples = require(workload, "samples", "workload")
    if not isinstance(samples, list) or any(
        not _finite_number(value) or value < 0 for value in samples
    ):
        errors.append("workload.samples must contain finite nonnegative numbers")
    updates = require(workload, "updates", "workload")
    if not isinstance(updates, int) or isinstance(updates, bool) or updates < 0:
        errors.append("workload.updates must be a nonnegative integer")
    events = require(workload, "events", "workload")
    if not isinstance(events, list):
        errors.append("workload.events must be a list")
    else:
        for index, event in enumerate(events):
            if not isinstance(event, dict) or not isinstance(event.get("type"), str):
                errors.append(f"workload.events[{index}] is invalid")
                continue
            if "atMs" in event and (
                not _finite_number(event["atMs"]) or event["atMs"] < 0
            ):
                errors.append(f"workload.events[{index}].atMs is invalid")
        if any(
            event.get("type") in ["blur", "resize", "visibilitychange"]
            for event in events
            if isinstance(event, dict)
        ):
            errors.append(
                "workload interval contains focus, resize, or visibility events"
            )
    workload_name = workload.get("name") if isinstance(workload, dict) else None
    if workload_name != "static":
        if not isinstance(updates, int) or isinstance(updates, bool) or updates <= 0:
            errors.append("active workload must complete at least one update")
        if not isinstance(samples, list) or not samples:
            errors.append("active workload must record at least one frame interval")

    process_metrics = require(report, "processMetrics", "report")
    if not isinstance(process_metrics, list):
        errors.append("processMetrics must be a list")
        process_metrics = []
    phase = expected.get("phase")
    interval_start = report.get("hostIntervalStart")
    interval_end = report.get("hostIntervalEnd")
    if process_metrics and (
        not _finite_number(interval_start)
        or not _finite_number(interval_end)
        or interval_start < 0
        or interval_end < interval_start
    ):
        errors.append("host sampling interval is invalid")
    if phase in ["timing", "memory"] and len(process_metrics) < 2:
        errors.append(f"{phase} phase requires at least two process samples")
    identities = []
    process_cpu_samples = []
    total_cpu_samples = []
    collector_identities = []
    collector_cpu_samples = []
    previous_time = None
    for index, sample in enumerate(process_metrics):
        if not isinstance(sample, dict):
            errors.append(f"processMetrics[{index}] must be an object")
            continue
        if not isinstance(sample.get("rootIdentity"), str):
            errors.append(f"processMetrics[{index}].rootIdentity is invalid")
        sample_time = sample.get("timeSeconds")
        if not _finite_number(sample_time) or sample_time < 0:
            errors.append(f"processMetrics[{index}].timeSeconds is invalid")
        elif previous_time is not None and sample_time < previous_time:
            errors.append("process metric time decreased")
        if (
            _finite_number(sample_time)
            and _finite_number(interval_start)
            and _finite_number(interval_end)
            and not interval_start <= sample_time <= interval_end
        ):
            errors.append(f"processMetrics[{index}] lies outside the host interval")
        previous_time = sample_time if _finite_number(sample_time) else previous_time
        processes = sample.get("processes")
        totals = sample.get("totals")
        collector_required = expected.get("collectorTelemetry") is True
        collector = sample.get("collector")
        sampling = sample.get("sampling")
        if collector is None:
            if collector_required:
                errors.append(f"processMetrics[{index}].collector is missing")
        elif not isinstance(collector, dict):
            errors.append(f"processMetrics[{index}].collector is invalid")
        elif not isinstance(collector.get("available"), bool):
            errors.append(f"processMetrics[{index}].collector.available is invalid")
        elif collector["available"]:
            pid = collector.get("pid")
            start_time = collector.get("startTimeTicks")
            identity = collector.get("identity")
            cpu_values = {
                key: collector.get(key)
                for key in ["userCpuSeconds", "systemCpuSeconds", "cpuSeconds"]
            }
            if (
                not isinstance(pid, int)
                or isinstance(pid, bool)
                or pid <= 0
                or not isinstance(start_time, int)
                or isinstance(start_time, bool)
                or start_time <= 0
                or identity != f"{pid}:{start_time}"
            ):
                errors.append(f"processMetrics[{index}].collector identity is invalid")
            if any(
                not _finite_number(value) or value < 0 for value in cpu_values.values()
            ) or (
                all(_finite_number(value) for value in cpu_values.values())
                and not math.isclose(
                    cpu_values["cpuSeconds"],
                    cpu_values["userCpuSeconds"] + cpu_values["systemCpuSeconds"],
                    rel_tol=1e-9,
                    abs_tol=1e-9,
                )
            ):
                errors.append(f"processMetrics[{index}].collector CPU is invalid")
            if isinstance(processes, dict) and (
                identity in processes
                or any(
                    isinstance(process, dict) and process.get("pid") == pid
                    for process in processes.values()
                )
            ):
                errors.append(
                    f"processMetrics[{index}].collector overlaps the target tree"
                )
            collector_identities.append(identity)
            collector_cpu_samples.append(cpu_values)
        else:
            nullable = [
                "pid",
                "startTimeTicks",
                "identity",
                "userCpuSeconds",
                "systemCpuSeconds",
                "cpuSeconds",
            ]
            if any(
                key not in collector or collector[key] is not None for key in nullable
            ):
                errors.append(
                    f"processMetrics[{index}].unavailable collector fields must be null"
                )
            if collector_required:
                errors.append(f"processMetrics[{index}].collector is unavailable")
        if sampling is None:
            if collector_required or collector is not None:
                errors.append(f"processMetrics[{index}].sampling is missing")
        elif not isinstance(sampling, dict) or any(
            not _finite_number(sampling.get(key)) or sampling[key] < 0
            for key in ["threadCpuSeconds", "wallSeconds"]
        ):
            errors.append(f"processMetrics[{index}].sampling is invalid")
        if not isinstance(totals, dict):
            errors.append(f"processMetrics[{index}].totals is invalid")
        else:
            for key in [
                "cpuSeconds",
                "userCpuSeconds",
                "systemCpuSeconds",
                "rssBytes",
                "fdCount",
                "fdCoverage",
                "fdInfoCoverage",
                "pssBytes",
                "privateBytes",
                "memoryCoverage",
            ]:
                value = totals.get(key)
                if not _finite_number(value) or value < 0:
                    errors.append(f"processMetrics[{index}].totals.{key} is invalid")
            total_cpu_samples.append({
                key: totals.get(key)
                for key in [
                    "cpuSeconds",
                    "userCpuSeconds",
                    "systemCpuSeconds",
                ]
            })
        if not isinstance(sample.get("drmClients"), dict):
            errors.append(f"processMetrics[{index}].drmClients is invalid")
        host = sample.get("host")
        if not isinstance(host, dict):
            errors.append(f"processMetrics[{index}].host is invalid")
        else:
            for name in [
                "wholeHostCpuTicks",
                "loadAverage",
                "platformProfile",
            ]:
                envelope = host.get(name)
                value = envelope.get("value") if isinstance(envelope, dict) else None
                value_valid = (
                    (
                        name == "wholeHostCpuTicks"
                        and isinstance(value, dict)
                        and all(
                            _finite_number(value.get(key)) and value[key] >= 0
                            for key in ["user", "system", "idle"]
                        )
                    )
                    or (
                        name == "loadAverage"
                        and isinstance(value, dict)
                        and all(
                            _finite_number(value.get(key)) and value[key] >= 0
                            for key in [
                                "oneMinute",
                                "fiveMinutes",
                                "fifteenMinutes",
                                "runnable",
                                "processes",
                            ]
                        )
                    )
                    or (
                        name == "platformProfile"
                        and isinstance(value, str)
                        and bool(value)
                    )
                )
                if (
                    not isinstance(envelope, dict)
                    or not isinstance(envelope.get("available"), bool)
                    or "value" not in envelope
                    or "error" not in envelope
                    or "reason" not in envelope
                    or (
                        envelope.get("available")
                        and (
                            not value_valid
                            or envelope.get("error") is not None
                            or envelope.get("reason") is not None
                        )
                    )
                    or (
                        not envelope.get("available")
                        and (
                            envelope.get("value") is not None
                            or not isinstance(envelope.get("error"), str)
                            or not isinstance(envelope.get("reason"), str)
                        )
                    )
                ):
                    errors.append(f"processMetrics[{index}].host.{name} is invalid")
            for name in [
                "cpuClocksKHz",
                "cpuGovernors",
                "temperaturesMilliC",
                "power",
            ]:
                envelope = host.get(name)
                if (
                    not isinstance(envelope, dict)
                    or not isinstance(envelope.get("available"), bool)
                    or not isinstance(envelope.get("values"), dict)
                    or not isinstance(envelope.get("errors"), dict)
                    or "reason" not in envelope
                    or (
                        envelope.get("available")
                        and (
                            not envelope.get("values")
                            or envelope.get("reason") is not None
                        )
                    )
                    or (
                        not envelope.get("available")
                        and (
                            envelope.get("values")
                            or not isinstance(envelope.get("reason"), str)
                        )
                    )
                ):
                    errors.append(f"processMetrics[{index}].host.{name} is invalid")
            background = host.get("backgroundProcessCpu")
            if (
                not isinstance(background, dict)
                or background.get("available") is not True
                or not isinstance(background.get("processCount"), int)
                or isinstance(background.get("processCount"), bool)
                or background.get("processCount", -1) < 0
                or not _finite_number(background.get("cpuSeconds"))
                or background.get("cpuSeconds", -1) < 0
                or background.get("reason") is not None
            ):
                errors.append(
                    f"processMetrics[{index}].host.backgroundProcessCpu is invalid"
                )
        if not isinstance(processes, dict) or not processes:
            errors.append(f"processMetrics[{index}].processes is missing")
            continue
        identities.append(set(processes))
        process_cpu_samples.append({
            identity: {
                key: process.get(key)
                for key in [
                    "cpuSeconds",
                    "userCpuSeconds",
                    "systemCpuSeconds",
                ]
            }
            for identity, process in processes.items()
            if isinstance(process, dict)
        })
        for identity, process in processes.items():
            if not isinstance(process, dict):
                errors.append(f"process {identity} is invalid")
                continue
            for key in ["cpuSeconds", "userCpuSeconds", "systemCpuSeconds", "rssBytes"]:
                value = process.get(key)
                if not _finite_number(value) or value < 0:
                    errors.append(f"process {identity} {key} is invalid")
            if identity != f"{process.get('pid')}:{process.get('startTimeTicks')}":
                errors.append(f"process {identity} identity is invalid")
            fd_count = process.get("fdCount")
            if fd_count is not None and (
                not isinstance(fd_count, int)
                or isinstance(fd_count, bool)
                or fd_count < 0
            ):
                errors.append(f"process {identity} fdCount is invalid")
            if fd_count is None and not process.get("errors"):
                errors.append(f"process {identity} fdCount coverage is unexplained")
            fdinfo_coverage = process.get("fdInfoCoverage")
            if (
                not isinstance(fdinfo_coverage, int)
                or isinstance(fdinfo_coverage, bool)
                or fdinfo_coverage < 0
                or (fd_count is not None and fdinfo_coverage > fd_count)
            ):
                errors.append(f"process {identity} fdInfoCoverage is invalid")
    if collector_identities and any(
        identity != collector_identities[0] for identity in collector_identities[1:]
    ):
        errors.append("collector identity changed")
    for before, after in zip(collector_cpu_samples, collector_cpu_samples[1:]):
        if any(
            _finite_number(before.get(key))
            and _finite_number(after.get(key))
            and after[key] < before[key]
            for key in ["userCpuSeconds", "systemCpuSeconds", "cpuSeconds"]
        ):
            errors.append("collector CPU counters decreased")
    stable_identities = identities and all(
        identity == identities[0] for identity in identities[1:]
    )
    if stable_identities:
        for index in range(1, len(process_cpu_samples)):
            for identity in identities[0]:
                before = process_cpu_samples[index - 1].get(identity, {})
                after = process_cpu_samples[index].get(identity, {})
                if any(
                    _finite_number(before.get(key))
                    and _finite_number(after.get(key))
                    and after[key] < before[key]
                    for key in [
                        "cpuSeconds",
                        "userCpuSeconds",
                        "systemCpuSeconds",
                    ]
                ):
                    errors.append(f"process {identity} CPU counters decreased")
        for index in range(1, len(total_cpu_samples)):
            if any(
                _finite_number(total_cpu_samples[index - 1].get(key))
                and _finite_number(total_cpu_samples[index].get(key))
                and total_cpu_samples[index][key] < total_cpu_samples[index - 1][key]
                for key in [
                    "cpuSeconds",
                    "userCpuSeconds",
                    "systemCpuSeconds",
                ]
            ):
                errors.append("total CPU counters decreased")
    if phase in ["timing", "memory"] and identities and not stable_identities:
        lifecycle = "lifecycle" in str(expected.get("workload", "")).lower()
        documented = isinstance(events, list) and any(
            isinstance(event, dict)
            and event.get("type") == "process-turnover"
            and event.get("cpuAttribution") == "lower-bound"
            for event in events
        )
        if not lifecycle or not documented:
            errors.append(f"{phase} process identities changed")
    if phase == "memory" and process_metrics:
        if any(
            sample.get("totals", {}).get("memoryCoverage", 0)
            < len(sample.get("processes", {}))
            or sample.get("totals", {}).get("fdCoverage", 0)
            < len(sample.get("processes", {}))
            or sample.get("totals", {}).get("fdInfoCoverage", 0)
            < sample.get("totals", {}).get("fdCount", 0)
            for sample in process_metrics
            if isinstance(sample, dict)
        ):
            errors.append("memory phase lacks full smaps/fd coverage")

    runtime = require(report, "runtime", "report")
    expected_runtime = expected.get("runtime")
    if runtime != expected_runtime:
        errors.append("runtime identity does not match the launch manifest")
    if not isinstance(runtime, dict):
        errors.append("runtime must be an object")
    else:
        for name in ["binary", "libxul"]:
            identity = runtime.get(name)
            if (
                not isinstance(identity, dict)
                or not isinstance(identity.get("path"), str)
                or not identity.get("path")
                or not _sha256(identity.get("sha256"))
            ):
                errors.append(f"runtime.{name} is invalid")

    mappings_before = require(report, "mappingsBefore", "report")
    mappings_after = require(report, "mappingsAfter", "report")
    hashes_before = require(report, "libraryHashesBefore", "report")
    hashes_after = require(report, "libraryHashesAfter", "report")
    if (
        not isinstance(mappings_before, list)
        or not mappings_before
        or any(not isinstance(path, str) or not path for path in mappings_before)
    ):
        errors.append("mappingsBefore must be a nonempty string list")
    if mappings_after != mappings_before:
        errors.append("runtime library mappings changed")
    if (
        not isinstance(hashes_before, dict)
        or not hashes_before
        or any(
            not isinstance(path, str) or not _sha256(digest)
            for path, digest in hashes_before.items()
        )
    ):
        errors.append("libraryHashesBefore is invalid")
    if hashes_after != hashes_before:
        errors.append("runtime library hashes changed")
    if isinstance(mappings_before, list) and isinstance(hashes_before, dict):
        if set(mappings_before) != set(hashes_before):
            errors.append("runtime mappings and library hashes differ")
        libxul = runtime.get("libxul") if isinstance(runtime, dict) else None
        if (
            not isinstance(libxul, dict)
            or libxul.get("path") not in mappings_before
            or hashes_before.get(libxul.get("path")) != libxul.get("sha256")
        ):
            errors.append("mapped libxul identity is invalid")
        if expected_backend == "vulkan" and not any(
            "libvulkan.so" in path for path in mappings_before
        ):
            errors.append("Vulkan runtime mapping is missing libvulkan.so")
        if expected_backend == "gl" and not any(
            token in path
            for path in mappings_before
            for token in ["libGL.so", "libEGL.so", "_dri.so", "libgallium"]
        ):
            errors.append("OpenGL runtime mapping is missing a GL driver library")

    pixels = require(report, "pixels", "report")
    if not isinstance(pixels, dict):
        errors.append("pixels must be an object")
    else:
        size = pixels.get("size")
        actual_pixels = pixels.get("pixels")
        checkpoint = pixels.get("checkpoint")
        if size != expected.get("viewport"):
            errors.append("pixel capture size mismatch")
        if not isinstance(checkpoint, dict):
            errors.append("pixel checkpoint is missing")
        else:
            points = checkpoint.get("points")
            expected_pixels = checkpoint.get("expected")
            expected_viewport = expected.get("viewport")
            if (
                isinstance(expected_viewport, list)
                and len(expected_viewport) == 2
                and all(isinstance(value, int) for value in expected_viewport)
            ):
                width, height = expected_viewport
            else:
                width = height = 0
            if (
                not isinstance(points, list)
                or not points
                or any(
                    not isinstance(point, list)
                    or len(point) != 2
                    or any(not isinstance(value, int) for value in point)
                    or not (0 <= point[0] < width and 0 <= point[1] < height)
                    for point in points
                )
            ):
                errors.append("pixel checkpoint points are invalid")
            if actual_pixels != expected_pixels:
                errors.append("captured pixels do not match the workload checkpoint")
            if (
                not isinstance(actual_pixels, list)
                or not actual_pixels
                or len(actual_pixels) != len(points or [])
                or any(
                    not isinstance(color, list)
                    or len(color) != 4
                    or any(
                        not isinstance(channel, int) or not 0 <= channel <= 255
                        for channel in color
                    )
                    for color in actual_pixels
                )
            ):
                errors.append("captured pixel samples are invalid")

    diagnostics = require(report, "diagnostics", "report")
    if not isinstance(diagnostics, list):
        errors.append("diagnostics must be a list")
        diagnostics = []
    needs_diagnostics = expected_backend == "vulkan" and phase == "diagnostic"
    if expected_backend == "gl" and phase == "diagnostic":
        errors.append("GL diagnostic phase has no HAL diagnostic schema")
    if needs_diagnostics and not diagnostics:
        errors.append("Vulkan diagnostic phase requires HAL diagnostics")
    if not needs_diagnostics and diagnostics:
        errors.append("HAL diagnostics are only allowed for Vulkan diagnostic phase")
    renderer_records = 0
    for index, record in enumerate(diagnostics):
        if not isinstance(record, dict) or HAL_ENVELOPE - record.keys():
            errors.append(f"diagnostics[{index}] is incomplete")
        elif record.get("version") != 1:
            errors.append(f"diagnostics[{index}] has unsupported version")
        else:
            if any(
                not isinstance(record.get(key), int)
                or isinstance(record.get(key), bool)
                or record[key] < 0
                for key in [
                    "pid",
                    "deviceId",
                    "rendererId",
                    "sequence",
                    "monotonicNs",
                ]
            ) or not isinstance(record.get("final"), bool):
                errors.append(f"diagnostics[{index}] metadata is invalid")
            if (
                isinstance(record.get("rendererId"), int)
                and not isinstance(record.get("rendererId"), bool)
                and record["rendererId"] > 0
            ):
                renderer_records += 1
            for key, names in [
                ("counters", HAL_COUNTERS),
                ("gauges", HAL_GAUGES),
                ("peaks", HAL_GAUGES),
                ("lastWorkNs", HAL_COUNTERS),
            ]:
                values = record.get(key)
                if not isinstance(values, dict) or names - values.keys():
                    errors.append(f"diagnostics[{index}].{key} is incomplete")
                elif any(
                    not isinstance(values[name], int)
                    or isinstance(values[name], bool)
                    or values[name] < 0
                    for name in names
                ):
                    errors.append(f"diagnostics[{index}].{key} is invalid")
    if needs_diagnostics and renderer_records == 0:
        errors.append("Vulkan diagnostic phase requires a renderer-scoped record")
    return errors
