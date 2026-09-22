# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import math
import os
import time

STARTUP_TIMEOUT_SECONDS = 120


def wait_for_startup(
    sampler,
    report,
    minimum_age=65,
    stable_seconds=5,
    timeout=STARTUP_TIMEOUT_SECONDS,
    clock=time.monotonic,
    sleep=time.sleep,
):
    if not all(
        math.isfinite(value) and value > 0
        for value in (minimum_age, stable_seconds, timeout)
    ):
        raise ValueError("Startup limits must be finite and positive")
    tick = os.sysconf("SC_CLK_TCK")
    started = clock()
    stable_since = None
    previous = {}
    root_identity = None
    report.update(
        passed=False,
        minimumAgeSeconds=minimum_age,
        requiredStableSeconds=stable_seconds,
        timeoutSeconds=timeout,
        transitions=[],
    )
    try:
        while True:
            sample = sampler.sample_once(detail="light")
            now = clock()
            root = sample["rootIdentity"]
            if root_identity is None:
                root_identity = root
            if root != root_identity or root not in sample["processes"]:
                raise RuntimeError(
                    "Startup root process identity changed or disappeared"
                )
            age = float((sampler.proc_root / "uptime").read_text().split()[0]) - (
                sample["processes"][root]["startTimeTicks"] / tick
            )
            if not math.isfinite(age) or age < 0:
                raise RuntimeError("Invalid startup process age")
            current = {
                identity: process["comm"]
                for identity, process in sample["processes"].items()
            }
            changed = current.keys() != previous.keys()
            if changed:
                report["transitions"].append({
                    "elapsedSeconds": now - started,
                    "rootAgeSeconds": age,
                    "added": {
                        key: current[key] for key in current.keys() - previous.keys()
                    },
                    "removed": {
                        key: previous[key] for key in previous.keys() - current.keys()
                    },
                })
            if age < minimum_age:
                stable_since = None
            elif changed or stable_since is None:
                stable_since = now
            previous = current
            report.update(
                rootIdentity=root,
                elapsedSeconds=now - started,
                rootAgeSeconds=age,
                stableSeconds=0 if stable_since is None else now - stable_since,
                finalIdentities=sorted(current),
            )
            if now - started >= timeout:
                raise TimeoutError("Firefox startup process tree did not settle")
            if stable_since is not None and now - stable_since >= stable_seconds:
                report["passed"] = True
                return
            sleep(min(sampler.interval, timeout - (now - started)))
    except Exception as error:
        report["error"] = str(error)
        raise
