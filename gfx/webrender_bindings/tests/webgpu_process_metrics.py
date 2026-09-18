# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import threading
import time
from pathlib import Path


def snapshot(root_pid, include_memory=True):
    processes = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
            processes[int(entry.name)] = fields
        except (OSError, IndexError):
            continue
    owned = {root_pid}
    while True:
        expanded = owned | {
            pid for pid, fields in processes.items() if int(fields[1]) in owned
        }
        if expanded == owned:
            break
        owned = expanded
    cpu = 0
    rss = 0
    pss = 0
    private = 0
    memory_processes = 0
    clients = {}
    for pid in owned:
        fields = processes.get(pid)
        if fields:
            cpu += int(fields[11]) + int(fields[12])
            rss += int(fields[21]) * os.sysconf("SC_PAGE_SIZE")
        if include_memory:
            try:
                rollup = dict(
                    line.split(":", 1)
                    for line in Path(f"/proc/{pid}/smaps_rollup")
                    .read_text()
                    .splitlines()
                    if ":" in line
                )
                pss += int(rollup["Pss"].split()[0]) * 1024
                private += sum(
                    int(rollup.get(key, "0").split()[0]) * 1024
                    for key in ["Private_Clean", "Private_Dirty"]
                )
                memory_processes += 1
            except (OSError, KeyError):
                pass
        try:
            descriptors = list(Path(f"/proc/{pid}/fdinfo").iterdir())
        except OSError:
            continue
        for descriptor in descriptors:
            try:
                data = dict(
                    line.split(":", 1)
                    for line in descriptor.read_text().splitlines()
                    if line.startswith("drm-")
                )
            except OSError:
                continue
            if "drm-client-id" not in data:
                continue
            identity = (
                data.get("drm-pdev", "global").strip()
                + "/"
                + data["drm-client-id"].strip()
            )
            clients[identity] = {key: value.strip() for key, value in data.items()}
    return {
        "time": time.monotonic(),
        "cpuSeconds": cpu / os.sysconf("SC_CLK_TCK"),
        "sumRssBytes": rss,
        "sumPssBytes": pss,
        "sumPrivateBytes": private,
        "memorySampledProcesses": memory_processes,
        "processes": len(owned),
        "drmClients": clients,
    }


class ProcessMetrics:
    def __init__(self, pid, include_memory=True):
        self.pid = pid
        self.include_memory = include_memory
        self.samples = []
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        while not self.stopped.is_set():
            self.samples.append(snapshot(self.pid, self.include_memory))
            self.stopped.wait(0.25)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *args):
        self.stopped.set()
        self.thread.join()
        self.samples.append(snapshot(self.pid, self.include_memory))
