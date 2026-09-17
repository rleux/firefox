# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import os
import re
import signal
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description="Run native VA-API/Vulkan tests without building")
    parser.add_argument("--exporter", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--clip", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--render-node", default="/dev/dri/renderD128")
    parser.add_argument("--icd", type=Path)
    parser.add_argument("--validation-layers", type=Path)
    parser.add_argument("--shader-input", choices=["native", "naga"])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    log = args.output / "native-tests.log"
    command = [str(args.exporter.resolve()), args.render_node, str(args.clip.resolve()),
               str(args.binary.resolve()), str(args.output.resolve())]
    env = os.environ.copy()
    if args.icd:
        env["VK_DRIVER_FILES"] = str(args.icd.resolve())
    if args.validation_layers:
        env["VK_LAYER_PATH"] = str(args.validation_layers.resolve())
        env["VK_LAYER_VALIDATE_SYNC"] = "1"
    if args.shader_input:
        env["WR_HAL_SHADER_INPUT"] = args.shader_input
    with log.open("w") as output:
        process = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT,
                                   start_new_session=True, env=env)
        try:
            result = process.wait(timeout=120)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            result = 124
    text = log.read_text(errors="replace")
    summary = re.search(r"test result: ok\. (\d+) passed; 0 failed", text)
    valid = summary and int(summary[1]) >= 4 and not any(
        marker in text for marker in ("VUID-", "VALIDATION [", "Validation Error")
    )
    print(f"Log: {log}")
    if result or not valid:
        print("Native video checks failed or Vulkan validation reported errors")
        return result or 1
    print(f"Passed {summary[1]} native video tests; no Vulkan validation errors")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
