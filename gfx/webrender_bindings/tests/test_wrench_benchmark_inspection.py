# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from wrench_benchmark_inspection import inspect_completed_measurement


class FakeClock:
    def __init__(self):
        self.now = 10.0
        self.sleeps = []

    def __call__(self):
        return self.now

    def sleep(self, duration):
        self.sleeps.append(duration)
        self.now += duration


class FakeProcess:
    def __init__(self):
        self.exited = False
        self.polls = 0

    def poll(self):
        self.polls += 1
        return 1 if self.exited else None


def write_result(path, *, completed=True, schema=1, inspection_ms=500, png=True):
    data = json.dumps({
        "schemaVersion": schema,
        "completed": completed,
        "inspectionMs": inspection_ms,
    }).encode()
    path.write_bytes(data)
    if png:
        path.with_suffix(".png").write_bytes(b"png")
    return data


class TestWrenchBenchmarkInspection(unittest.TestCase):
    def run_inspection(
        self,
        path,
        process,
        capture,
        evidence,
        clock,
        *,
        timeout=1,
        inspection_ms=500,
    ):
        return inspect_completed_measurement(
            process,
            path,
            capture,
            evidence,
            inspection_ms=inspection_ms,
            timeout=timeout,
            clock=clock,
            sleep=clock.sleep,
            poll_interval=0.025,
        )

    def test_retries_only_missing_and_partial_json_then_captures_twice(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            process = FakeProcess()
            clock = FakeClock()
            evidence = {}
            payloads = []
            sleep_count = 0
            original_sleep = clock.sleep

            def staged_sleep(duration):
                nonlocal sleep_count
                sleep_count += 1
                if sleep_count == 1:
                    result.write_text("{")
                elif sleep_count == 2:
                    write_result(result)
                original_sleep(duration)

            clock.sleep = staged_sleep

            def capture():
                self.assertTrue(json.loads(result.read_text())["completed"])
                self.assertTrue(result.with_suffix(".png").is_file())
                self.assertIsNotNone(evidence["readyTimeSeconds"])
                payloads.append({"time": clock()})
                clock.now += 0.01
                return "ignored"

            self.assertIsNone(
                self.run_inspection(result, process, capture, evidence, clock)
            )
            self.assertEqual(len(payloads), 2)
            self.assertTrue(evidence["passed"])
            self.assertEqual(evidence["mode"], "post-result")
            self.assertGreater(
                evidence["readyTimeSeconds"], evidence["startedTimeSeconds"]
            )
            self.assertEqual(
                evidence["resultSha256"],
                hashlib.sha256(result.read_bytes()).hexdigest(),
            )
            self.assertIn("Expecting property name", evidence["lastReadinessError"])
            self.assertEqual(len(evidence["captures"]), 2)
            self.assertTrue(
                all(
                    payload["time"] >= evidence["readyTimeSeconds"]
                    for payload in payloads
                )
            )
            for entry in evidence["captures"]:
                self.assertTrue(entry["completed"])
                self.assertGreaterEqual(
                    entry["endedTimeSeconds"], entry["startedTimeSeconds"]
                )
            self.assertGreaterEqual(
                evidence["endedTimeSeconds"],
                evidence["captures"][-1]["endedTimeSeconds"],
            )

    def test_complete_invalid_result_is_not_retried_or_captured(self):
        cases = [
            {"completed": False},
            {"schema": 2},
            {"inspection_ms": 499},
            {"png": False},
        ]
        for fields in cases:
            with self.subTest(
                fields=fields
            ), tempfile.TemporaryDirectory() as temporary:
                result = Path(temporary) / "measurement.json"
                write_result(result, **fields)
                process = FakeProcess()
                clock = FakeClock()
                evidence = {}
                captures = []
                with self.assertRaises(ValueError):
                    self.run_inspection(
                        result, process, lambda: captures.append(True), evidence, clock
                    )
                self.assertEqual(captures, [])
                self.assertFalse(evidence["passed"])
                self.assertIn("error", evidence)
                self.assertEqual(clock.sleeps, [])

    def test_large_completed_result_is_fully_parsed_before_capture(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            data = json.dumps({
                "schemaVersion": 1,
                "completed": True,
                "inspectionMs": 500,
                "samples": [
                    {
                        "sceneReadyNs": index,
                        "renderCallNs": index + 1,
                        "completionReadbackNs": index + 2,
                        "totalNs": index * 3 + 3,
                    }
                    for index in range(5000)
                ],
            }).encode()
            result.write_bytes(data)
            result.with_suffix(".png").write_bytes(b"png")
            captures = []
            evidence = {}
            self.assertIsNone(
                self.run_inspection(
                    result,
                    FakeProcess(),
                    lambda: captures.append(True),
                    evidence,
                    FakeClock(),
                )
            )
            self.assertEqual(captures, [True, True])
            self.assertEqual(evidence["resultSha256"], hashlib.sha256(data).hexdigest())

    def test_missing_result_times_out_and_preserves_readiness_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            process = FakeProcess()
            clock = FakeClock()
            evidence = {}
            captures = []
            with self.assertRaises(TimeoutError):
                self.run_inspection(
                    result,
                    process,
                    lambda: captures.append(True),
                    evidence,
                    clock,
                    timeout=0.06,
                )
            self.assertEqual(captures, [])
            self.assertFalse(evidence["passed"])
            self.assertIn("No such file", evidence["lastReadinessError"])
            self.assertIn("did not become available", evidence["error"])
            self.assertGreaterEqual(evidence["endedTimeSeconds"], 10.06)

    def test_process_exit_before_readiness_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            process = FakeProcess()
            process.exited = True
            clock = FakeClock()
            evidence = {}
            with self.assertRaisesRegex(RuntimeError, "before live result inspection"):
                self.run_inspection(result, process, lambda: None, evidence, clock)
            self.assertEqual(evidence["captures"], [])
            self.assertIn("before live result inspection", evidence["error"])

    def test_process_exit_during_capture_fails_and_keeps_capture_entry(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            write_result(result)
            process = FakeProcess()
            clock = FakeClock()
            evidence = {}

            def capture():
                process.exited = True
                clock.now += 0.01

            with self.assertRaisesRegex(RuntimeError, "during live capture"):
                self.run_inspection(result, process, capture, evidence, clock)
            self.assertEqual(len(evidence["captures"]), 1)
            self.assertFalse(evidence["captures"][0]["completed"])
            self.assertGreaterEqual(
                evidence["captures"][0]["endedTimeSeconds"],
                evidence["captures"][0]["startedTimeSeconds"],
            )
            self.assertIn("during live capture", evidence["error"])

    def test_process_exit_before_second_capture_preserves_first(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            write_result(result)
            clock = FakeClock()
            evidence = {}
            captures = []

            class ExitBeforeSecondCapture(FakeProcess):
                def poll(self):
                    self.polls += 1
                    return 1 if self.polls >= 4 else None

            with self.assertRaisesRegex(RuntimeError, "before live capture"):
                self.run_inspection(
                    result,
                    ExitBeforeSecondCapture(),
                    lambda: captures.append(True),
                    evidence,
                    clock,
                )
            self.assertEqual(captures, [True])
            self.assertEqual(len(evidence["captures"]), 1)
            self.assertTrue(evidence["captures"][0]["completed"])
            self.assertIn("before live capture", evidence["error"])

    def test_timeout_during_capture_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            write_result(result)
            clock = FakeClock()
            evidence = {}

            def capture():
                clock.now += 1

            with self.assertRaisesRegex(TimeoutError, "exceeded its timeout"):
                self.run_inspection(
                    result,
                    FakeProcess(),
                    capture,
                    evidence,
                    clock,
                    timeout=0.5,
                )
            self.assertEqual(len(evidence["captures"]), 1)
            self.assertFalse(evidence["captures"][0]["completed"])
            self.assertIn("exceeded its timeout", evidence["error"])

    def test_already_ready_result_captures_without_sleep(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            write_result(result)
            clock = FakeClock()
            evidence = {}
            captures = []
            self.assertIsNone(
                self.run_inspection(
                    result,
                    FakeProcess(),
                    lambda: captures.append(True),
                    evidence,
                    clock,
                )
            )
            self.assertEqual(clock.sleeps, [])
            self.assertEqual(captures, [True, True])
            self.assertEqual(
                evidence["readyTimeSeconds"], evidence["startedTimeSeconds"]
            )

    def test_capture_exception_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            write_result(result)
            process = FakeProcess()
            clock = FakeClock()
            evidence = {}

            def capture():
                raise OSError("capture failed")

            with self.assertRaisesRegex(OSError, "capture failed"):
                self.run_inspection(result, process, capture, evidence, clock)
            self.assertEqual(len(evidence["captures"]), 1)
            self.assertFalse(evidence["captures"][0]["completed"])
            self.assertIn("capture failed", evidence["error"])

    def test_limits_are_validated_before_evidence_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = Path(temporary) / "measurement.json"
            for values in [
                {"inspection_ms": 0},
                {"timeout": float("nan")},
                {"timeout": 0},
            ]:
                with self.subTest(values=values):
                    evidence = {}
                    with self.assertRaises(ValueError):
                        self.run_inspection(
                            result,
                            FakeProcess(),
                            lambda: None,
                            evidence,
                            FakeClock(),
                            **values,
                        )
                    self.assertEqual(evidence, {})


if __name__ == "__main__":
    unittest.main()
