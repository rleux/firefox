# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from renderer_benchmark_perf import (
    PerfRecorder,
    parse_event_attributes,
    parse_perf_script,
    parse_tracking_build_id,
)

EVENT_LINE = (
    "cpu-clock:uk: type: 1 (PERF_TYPE_SOFTWARE), size: 136, config: 0 "
    "(PERF_COUNT_SW_CPU_CLOCK), { sample_period, sample_freq }: 99, "
    "sample_type: IP|TID|TIME|ADDR|CALLCHAIN|CPU|PERIOD|REGS_USER|STACK_USER, "
    "read_format: ID|LOST, disabled: 1, inherit: 1, exclude_hv: 1, freq: 1, "
    "sample_id_all: 1, use_clockid: 1, sample_stack_user: 16384, clockid: 1"
)
TRACKING_LINE = (
    "dummy:uH: type: 1, config: 0x9, sample_type: IP|TID|TIME, inherit: 1, build_id: 1"
)


def mmap_line(path, build_id):
    return (
        "0.000000000: PERF_RECORD_MMAP2 10/10: "
        f"[0x400000(0x1000) @ 0 <{build_id}>]: r-xp {path}"
    )


class FakeProcess:
    def __init__(self, statuses=None):
        self.statuses = list(statuses or [])

    def poll(self):
        return self.statuses.pop(0) if self.statuses else None


def recorder(temporary, timeout=1):
    evidence = {"controls": []}
    value = PerfRecorder(
        "/usr/bin/perf",
        10,
        Path(temporary),
        evidence,
        timeout=timeout,
    )
    value.process = FakeProcess()
    value.control_fd = 11
    value.ack_fd = 12
    return value, evidence


def full_write(_fd, payload):
    return len(payload)


class PerfAttributeTests(unittest.TestCase):
    def test_parser_normalizes_omitted_zero_flags(self):
        attributes = parse_event_attributes(EVENT_LINE + "\n")
        self.assertEqual(attributes["name"], "cpu-clock:uk")
        self.assertEqual(attributes["type"], 1)
        self.assertEqual(attributes["config"], 0)
        self.assertEqual(attributes["frequencyHz"], 99)
        self.assertEqual(attributes["frequencyMode"], 1)
        self.assertEqual(attributes["excludeUser"], 0)
        self.assertEqual(attributes["excludeKernel"], 0)
        self.assertEqual(attributes["excludeHypervisor"], 1)
        self.assertEqual(attributes["inherit"], 1)
        self.assertEqual(attributes["stackBytes"], 16384)
        self.assertEqual(attributes["clockId"], 1)
        self.assertTrue(
            {"IP", "TID", "TIME", "CPU", "PERIOD", "REGS_USER", "STACK_USER"}
            <= set(attributes["sampleTypes"])
        )

    def test_parser_rejects_missing_ambiguous_and_incomplete_events(self):
        for text in ["", EVENT_LINE + "\n" + EVENT_LINE, "cpu-clock:uk: type: 1"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                parse_event_attributes(text)

    def test_tracking_build_id_parser(self):
        self.assertEqual(parse_tracking_build_id(TRACKING_LINE), 1)
        self.assertEqual(
            parse_tracking_build_id(TRACKING_LINE.replace("build_id: 1", "")), 0
        )
        with self.assertRaises(ValueError):
            parse_tracking_build_id("")

    def test_build_id_and_sample_time_parsers(self):
        build_ids, sample_times = parse_perf_script(
            mmap_line("/tiny", "ab")
            + "\n"
            + mmap_line("/snapshot/firefox", "a" * 40)
            + "\n"
            + mmap_line("/snapshot/libxul.so", "b" * 32)
            + "\n20.100000000:\n20.200000000: \n"
        )
        self.assertEqual(build_ids["/tiny"], "ab")
        self.assertEqual(build_ids["/snapshot/firefox"], "a" * 40)
        self.assertEqual(build_ids["/snapshot/libxul.so"], "b" * 32)
        self.assertEqual(sample_times, [20.1, 20.2])
        for text in [
            "bad",
            mmap_line("/odd", "abc") + "\n20.1:\n",
            mmap_line("/long", "a" * 42) + "\n20.1:\n",
            mmap_line("/zero", "0" * 40) + "\n20.1:\n",
            mmap_line("/dso", "a" * 40)
            + "\n"
            + mmap_line("/dso", "b" * 40)
            + "\n20.1:\n",
            "20.2:\n20.1:\n",
        ]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                parse_perf_script(text)


class PerfControlTests(unittest.TestCase):
    def test_fragmented_acknowledgement_is_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary)
            readable = (value.ack_fd,), (), ()
            with patch(
                "renderer_benchmark_perf.select.select",
                side_effect=[
                    ((), (), ()),
                    readable,
                    readable,
                    readable,
                    ((), (), ()),
                ],
            ):
                with patch("renderer_benchmark_perf.os.write", side_effect=full_write):
                    with patch(
                        "renderer_benchmark_perf.os.read",
                        side_effect=[b"a", b"ck\n", b"\0"],
                    ):
                        value.command("ping")
            self.assertNotIn("error", evidence)
            self.assertEqual(
                evidence["controls"][0]["acknowledgementHex"], "61636b0a00"
            )
            self.assertGreaterEqual(
                evidence["controls"][0]["acknowledgedTimeSeconds"],
                evidence["controls"][0]["requestedTimeSeconds"],
            )

    def test_stale_acknowledgement_is_rejected_before_write(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary)
            with patch(
                "renderer_benchmark_perf.select.select",
                return_value=((value.ack_fd,), (), ()),
            ):
                with patch("renderer_benchmark_perf.os.write") as write:
                    with self.assertRaisesRegex(RuntimeError, "stale"):
                        value.command("enable")
            write.assert_not_called()
            self.assertIn("stale", evidence["error"])

    def test_malformed_and_extra_acknowledgements_are_rejected(self):
        for response in [b"bad", b"ack\n\0extra"]:
            with self.subTest(
                response=response
            ), tempfile.TemporaryDirectory() as temporary:
                value, evidence = recorder(temporary)
                with patch(
                    "renderer_benchmark_perf.select.select",
                    side_effect=[
                        ((), (), ()),
                        ((value.ack_fd,), (), ()),
                    ],
                ):
                    with patch(
                        "renderer_benchmark_perf.os.write", side_effect=full_write
                    ):
                        with patch(
                            "renderer_benchmark_perf.os.read", return_value=response
                        ):
                            with self.assertRaisesRegex(
                                RuntimeError, "Invalid perf acknowledgement"
                            ):
                                value.command("disable")
                self.assertIn("Invalid perf acknowledgement", evidence["error"])

    def test_short_control_write_and_split_extra_ack_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary)
            with patch(
                "renderer_benchmark_perf.select.select",
                return_value=((), (), ()),
            ):
                with patch("renderer_benchmark_perf.os.write", return_value=4):
                    with self.assertRaisesRegex(
                        RuntimeError, "Short perf control write"
                    ):
                        value.command("enable")
            self.assertIn("Short perf control write", evidence["error"])

        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary)
            with patch(
                "renderer_benchmark_perf.select.select",
                side_effect=[
                    ((), (), ()),
                    ((value.ack_fd,), (), ()),
                    ((value.ack_fd,), (), ()),
                ],
            ):
                with patch("renderer_benchmark_perf.os.write", side_effect=full_write):
                    with patch(
                        "renderer_benchmark_perf.os.read", return_value=b"ack\n\0"
                    ):
                        with self.assertRaisesRegex(
                            RuntimeError, "extra perf acknowledgement"
                        ):
                            value.command("enable")
            self.assertIn("extra perf acknowledgement", evidence["error"])

    def test_acknowledgement_timeout_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary, timeout=0.001)
            with patch(
                "renderer_benchmark_perf.select.select", return_value=((), (), ())
            ):
                with patch("renderer_benchmark_perf.os.write", side_effect=full_write):
                    with self.assertRaisesRegex(TimeoutError, "timed out"):
                        value.command("stop")
            self.assertIn("timed out", evidence["error"])

    def test_recorder_exit_before_and_during_acknowledgement_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary)
            value.process = FakeProcess([1])
            with self.assertRaisesRegex(RuntimeError, "before control command"):
                value.command("ping")
            self.assertIn("before control command", evidence["error"])

        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = recorder(temporary)
            value.process = FakeProcess([None, 1])
            with patch(
                "renderer_benchmark_perf.select.select", return_value=((), (), ())
            ):
                with patch("renderer_benchmark_perf.os.write", side_effect=full_write):
                    with self.assertRaisesRegex(RuntimeError, "before acknowledging"):
                        value.command("ping")
            self.assertIn("before acknowledging", evidence["error"])

    def test_close_unlinks_owned_fifos(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, _ = recorder(temporary)
            paths = [Path(temporary) / "control", Path(temporary) / "ack"]
            for path in paths:
                os.mkfifo(path)
            value.fifos.extend(paths)
            value.process = None
            value.control_fd = None
            value.ack_fd = None
            value.close()
            self.assertEqual(value.fifos, [])
            self.assertTrue(all(not path.exists() for path in paths))


class PerfFinalizeTests(unittest.TestCase):
    def make_recorder(self, temporary):
        evidence = {
            "controls": [
                {
                    "command": "ping",
                    "requestedTimeSeconds": 19.7,
                    "acknowledgedTimeSeconds": 19.8,
                },
                {
                    "command": "enable",
                    "requestedTimeSeconds": 20.0,
                    "acknowledgedTimeSeconds": 20.01,
                },
                {
                    "command": "disable",
                    "requestedTimeSeconds": 20.3,
                    "acknowledgedTimeSeconds": 20.31,
                },
            ]
        }
        value = PerfRecorder(
            "/usr/bin/perf",
            10,
            Path(temporary),
            evidence,
            timeout=10,
            finalize_timeout=120,
        )
        value.process = Mock()
        (Path(temporary) / "perf.data").write_bytes(b"perf data")
        return value, evidence

    def test_finish_uses_separate_finalization_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = self.make_recorder(temporary)
            value.process.wait.return_value = 0

            def run_perf(command_line, **kwargs):
                if command_line[1] == "evlist":
                    return SimpleNamespace(
                        stdout=EVENT_LINE + "\n" + TRACKING_LINE + "\n"
                    )
                kwargs["stdout"].write(
                    mmap_line("/snapshot/firefox", "a" * 40)
                    + "\n20.100000000:\n20.200000000:\n"
                )
                return SimpleNamespace()

            with patch.object(value, "check_target"), patch.object(
                value, "command"
            ) as command, patch.object(value, "close"), patch(
                "renderer_benchmark_perf.time.monotonic", side_effect=[20.0, 20.5]
            ), patch(
                "renderer_benchmark_perf.subprocess.run",
                side_effect=run_perf,
            ) as run:
                value.finish()
            command.assert_called_once_with("stop")
            value.process.wait.assert_called_once_with(timeout=120)
            self.assertEqual(evidence["finalizeStartedTimeSeconds"], 20.0)
            self.assertEqual(evidence["finalizeEndedTimeSeconds"], 20.5)
            self.assertEqual(evidence["returncode"], 0)
            self.assertEqual(evidence["trackingBuildId"], 1)
            self.assertEqual(evidence["sampleCount"], 2)
            self.assertEqual(evidence["firstSampleTimeSeconds"], 20.1)
            self.assertEqual(evidence["lastSampleTimeSeconds"], 20.2)
            self.assertTrue(evidence["passed"])
            self.assertEqual(run.call_count, 2)

    def test_finalization_timeout_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            value, evidence = self.make_recorder(temporary)
            value.process.wait.side_effect = subprocess.TimeoutExpired(
                ["perf", "record"], 120
            )
            with patch.object(value, "check_target"), patch.object(
                value, "command"
            ), patch.object(value, "close"), patch(
                "renderer_benchmark_perf.time.monotonic", side_effect=[30.0, 150.0]
            ), self.assertRaises(subprocess.TimeoutExpired):
                value.finish()
            value.process.wait.assert_called_once_with(timeout=120)
            self.assertEqual(evidence["finalizeStartedTimeSeconds"], 30.0)
            self.assertEqual(evidence["finalizeEndedTimeSeconds"], 150.0)
            self.assertIn("timed out after 120 seconds", evidence["error"])
            self.assertNotIn("returncode", evidence)


if __name__ == "__main__":
    unittest.main()
