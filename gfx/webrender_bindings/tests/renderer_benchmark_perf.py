# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import hashlib
import math
import os
import re
import select
import subprocess
import time
from pathlib import Path

from renderer_benchmark_metrics import _read_stat

PERF_FINALIZE_TIMEOUT_SECONDS = 120


def parse_event_attributes(text):
    events = [line for line in text.splitlines() if line.startswith("cpu-clock")]
    if len(events) != 1 or ": type:" not in events[0]:
        raise ValueError("Expected exactly one recorded cpu-clock event")
    line = events[0]
    fields = {
        name: int(value, 0)
        for name, value in re.findall(r"\b([a-z_]+): (0x[0-9a-f]+|[0-9]+)\b", line)
    }
    frequency = re.search(r"\{ sample_period, sample_freq \}: ([0-9]+)", line)
    sample_types = re.search(r"\bsample_type: ([A-Z_|]+)", line)
    if not frequency or not sample_types:
        raise ValueError("Recorded perf sampling attributes are incomplete")
    return {
        "name": line.split(": type:", 1)[0],
        "type": fields.get("type"),
        "config": fields.get("config"),
        "frequencyHz": int(frequency.group(1)),
        "frequencyMode": fields.get("freq", 0),
        "excludeUser": fields.get("exclude_user", 0),
        "excludeKernel": fields.get("exclude_kernel", 0),
        "excludeHypervisor": fields.get("exclude_hv", 0),
        "inherit": fields.get("inherit", 0),
        "stackBytes": fields.get("sample_stack_user", 0),
        "clockId": fields.get("clockid"),
        "sampleTypes": sample_types.group(1).split("|"),
    }


class PerfRecorder:
    def __init__(
        self,
        binary,
        pid,
        output,
        evidence,
        event="cpu-clock:uk",
        timeout=10,
        finalize_timeout=PERF_FINALIZE_TIMEOUT_SECONDS,
    ):
        if event not in ("cpu-clock:uk", "cpu-clock:u"):
            raise ValueError("Unsupported profiling event")
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("Perf timeout must be finite and positive")
        if not math.isfinite(finalize_timeout) or finalize_timeout <= 0:
            raise ValueError("Perf finalization timeout must be finite and positive")
        self.binary = Path(binary)
        self.pid = pid
        self.output = Path(output)
        self.evidence = evidence
        self.event = event
        self.timeout = timeout
        self.finalize_timeout = finalize_timeout
        self.process = None
        self.log = None
        self.control_fd = None
        self.ack_fd = None
        self.identity = None
        self.recording = False
        self.fifos = []

    def check_target(self):
        stat = _read_stat(Path(f"/proc/{self.pid}/stat"))
        identity = f"{self.pid}:{stat['startTimeTicks']}"
        if self.identity is None:
            self.identity = identity
        elif identity != self.identity:
            raise RuntimeError("Perf target process identity changed")

    def command(self, name):
        try:
            self._command(name)
        except BaseException as error:
            self.evidence["error"] = str(error)
            raise

    def _command(self, name):
        if self.process.poll() is not None:
            raise RuntimeError("Perf recorder exited before control command")
        if select.select([self.ack_fd], [], [], 0)[0]:
            raise RuntimeError("Unexpected stale perf acknowledgement")
        record = {"command": name, "requestedTimeSeconds": time.monotonic()}
        self.evidence["controls"].append(record)
        payload = (name + "\n").encode()
        if os.write(self.control_fd, payload) != len(payload):
            raise RuntimeError("Short perf control write")
        deadline = time.monotonic() + self.timeout
        response = b""
        # evlist__ctlfd_ack writes sizeof("ack\n"), including the trailing NUL.
        acknowledgement = b"ack\n\0"
        while len(response) < len(acknowledgement):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"Perf {name} acknowledgement timed out")
            if select.select([self.ack_fd], [], [], min(remaining, 0.1))[0]:
                response += os.read(self.ack_fd, 4096)
                record["acknowledgementHex"] = response.hex()
                if not acknowledgement.startswith(response):
                    raise RuntimeError(f"Invalid perf acknowledgement: {response!r}")
            elif self.process.poll() is not None:
                raise RuntimeError(f"Perf exited before acknowledging {name}")
        if select.select([self.ack_fd], [], [], 0)[0]:
            raise RuntimeError("Unexpected extra perf acknowledgement bytes")
        record["acknowledgedTimeSeconds"] = time.monotonic()

    def start(self):
        self.evidence.update(
            passed=False,
            event=self.event,
            frequencyHz=99,
            callGraph="dwarf,16384",
            controlTimeoutSeconds=self.timeout,
            finalizeTimeoutSeconds=self.finalize_timeout,
            buildIdMode="all-dsos",
            controls=[],
        )
        try:
            self.check_target()
            with self.binary.open("rb") as stream:
                self.evidence["binarySha256"] = hashlib.file_digest(
                    stream, "sha256"
                ).hexdigest()
            self.evidence["binary"] = str(self.binary)
            self.evidence["targetIdentity"] = self.identity
            self.evidence["targetPid"] = self.pid
            control = self.output / "perf-control.fifo"
            ack = self.output / "perf-ack.fifo"
            if any(
                delimiter in str(path)
                for path in (control, ack)
                for delimiter in (",", "\n")
            ):
                raise ValueError("Perf FIFO paths contain a control delimiter")
            for path in (control, ack):
                os.mkfifo(path, 0o600)
                self.fifos.append(path)
            flags = os.O_RDWR | os.O_NONBLOCK | os.O_CLOEXEC
            self.control_fd = os.open(control, flags)
            self.ack_fd = os.open(ack, flags)
            command = [
                str(self.binary),
                "record",
                "-e",
                self.event,
                "-F",
                "99",
                "--strict-freq",
                "--call-graph",
                "dwarf,16384",
                "--clockid",
                "mono",
                "--no-buildid-cache",
                "--buildid-all",
                "-p",
                str(self.pid),
                "-D",
                "-1",
                "--control",
                f"fifo:{control},{ack}",
                "--timestamp",
                "--timestamp-boundary",
                "--sample-cpu",
                "-o",
                str(self.output / "perf.data"),
            ]
            self.evidence["command"] = command
            self.log = (self.output / "perf-record.log").open("x")
            self.process = subprocess.Popen(
                command,
                stdout=self.log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            self.evidence["recorderPid"] = self.process.pid
            self.command("ping")
            self.check_target()
        except BaseException as error:
            self.evidence["error"] = str(error)
            self.close()
            raise

    def enable(self):
        self.check_target()
        self.command("enable")
        self.recording = True

    def disable(self):
        if not self.recording:
            raise RuntimeError("Perf recording was not enabled")
        self.command("disable")
        self.recording = False
        self.check_target()

    def finish(self):
        try:
            if self.recording or [
                value["command"] for value in self.evidence["controls"]
            ] != ["ping", "enable", "disable"]:
                raise RuntimeError("Perf must be disabled before finalization")
            self.check_target()
            self.command("stop")
            self.evidence["finalizeStartedTimeSeconds"] = time.monotonic()
            try:
                status = self.process.wait(timeout=self.finalize_timeout)
            finally:
                self.evidence["finalizeEndedTimeSeconds"] = time.monotonic()
            self.evidence["returncode"] = status
            if status != 0:
                raise RuntimeError(f"Perf exited with status {status}")
            path = self.output / "perf.data"
            size = path.stat().st_size
            if not size:
                raise RuntimeError("Perf data is empty")
            with path.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            self.evidence.update(dataPath=str(path), dataBytes=size, dataSha256=digest)
            result = subprocess.run(
                [str(self.binary), "evlist", "-v", "-i", str(path)],
                check=True,
                capture_output=True,
                text=True,
                timeout=self.timeout,
            )
            (self.output / "perf-event-attributes.txt").write_text(result.stdout)
            attributes = parse_event_attributes(result.stdout)
            self.evidence["eventAttributes"] = attributes
            if (
                attributes["type"] != 1
                or attributes["config"] != 0
                or attributes["frequencyHz"] != 99
                or attributes["frequencyMode"] != 1
                or attributes["excludeUser"] != 0
                or attributes["excludeKernel"] != int(self.event == "cpu-clock:u")
                or attributes["inherit"] != 1
                or attributes["stackBytes"] != 16384
                or attributes["clockId"] != 1
                or not {"IP", "TID", "TIME", "CPU", "PERIOD", "REGS_USER", "STACK_USER"}
                <= set(attributes["sampleTypes"])
            ):
                raise RuntimeError("Recorded perf event attributes differ from request")
            self.evidence["passed"] = True
        except BaseException as error:
            self.evidence["error"] = str(error)
            raise
        finally:
            self.close()

    def close(self):
        try:
            if self.process is not None:
                from run_renderer_benchmark import stop

                stop(self.process, grace=self.timeout)
        finally:
            if self.log is not None:
                self.log.close()
                self.log = None
            for name in ("control_fd", "ack_fd"):
                fd = getattr(self, name)
                if fd is not None:
                    os.close(fd)
                    setattr(self, name, None)
            for path in self.fifos:
                path.unlink(missing_ok=True)
            self.fifos.clear()
