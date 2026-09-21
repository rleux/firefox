# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from renderer_benchmark_perf import PerfRecorder, parse_event_attributes

EVENT_LINE = (
    "cpu-clock:uk: type: 1 (PERF_TYPE_SOFTWARE), size: 136, config: 0 "
    "(PERF_COUNT_SW_CPU_CLOCK), { sample_period, sample_freq }: 99, "
    "sample_type: IP|TID|TIME|ADDR|CALLCHAIN|CPU|PERIOD|REGS_USER|STACK_USER, "
    "read_format: ID|LOST, disabled: 1, inherit: 1, exclude_hv: 1, freq: 1, "
    "sample_id_all: 1, use_clockid: 1, sample_stack_user: 16384, clockid: 1"
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


if __name__ == "__main__":
    unittest.main()
