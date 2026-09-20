# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import hashlib
import json
import math
import time


def inspect_completed_measurement(
    process,
    result_path,
    capture,
    evidence,
    *,
    inspection_ms,
    timeout,
    clock=time.monotonic,
    sleep=time.sleep,
    poll_interval=0.025,
):
    if (
        not math.isfinite(timeout)
        or timeout <= 0
        or not math.isfinite(poll_interval)
        or poll_interval <= 0
        or not isinstance(inspection_ms, int)
        or isinstance(inspection_ms, bool)
        or inspection_ms <= 0
    ):
        raise ValueError("Inspection requires positive finite limits and a hold")
    started = clock()
    evidence.update(
        mode="post-result",
        passed=False,
        pollIntervalSeconds=poll_interval,
        timeoutSeconds=timeout,
        inspectionMs=inspection_ms,
        startedTimeSeconds=started,
        readyTimeSeconds=None,
        captures=[],
        lastReadinessError=None,
    )
    try:
        while True:
            if clock() - started >= timeout:
                raise TimeoutError("Completed Wrench result did not become available")
            if process.poll() is not None:
                raise RuntimeError("Wrench exited before live result inspection")
            try:
                data = result_path.read_bytes()
                report = json.loads(data)
            except (
                FileNotFoundError,
                json.JSONDecodeError,
                UnicodeDecodeError,
            ) as error:
                evidence["lastReadinessError"] = str(error)
            else:
                if not isinstance(report, dict):
                    raise ValueError("Wrench result must be an object")
                if (
                    report.get("completed") is not True
                    or report.get("schemaVersion") != 1
                    or report.get("inspectionMs") != inspection_ms
                ):
                    raise ValueError("Wrench inspection result envelope mismatch")
                elif not result_path.with_suffix(".png").is_file():
                    raise ValueError("Wrench result PNG is missing")
                else:
                    evidence["readyTimeSeconds"] = clock()
                    evidence["resultSha256"] = hashlib.sha256(data).hexdigest()
                    break
            sleep(min(poll_interval, max(0, timeout - (clock() - started))))

        for _ in range(2):
            if process.poll() is not None:
                raise RuntimeError("Wrench exited before live capture")
            entry = {"startedTimeSeconds": clock(), "completed": False}
            evidence["captures"].append(entry)
            try:
                capture()
                if process.poll() is not None:
                    raise RuntimeError("Wrench exited during live capture")
                if clock() - started >= timeout:
                    raise TimeoutError("Wrench inspection exceeded its timeout")
                entry["completed"] = True
            finally:
                entry["endedTimeSeconds"] = clock()
        evidence["passed"] = True
    except Exception as error:
        evidence["error"] = str(error)
        raise
    finally:
        evidence["endedTimeSeconds"] = clock()
