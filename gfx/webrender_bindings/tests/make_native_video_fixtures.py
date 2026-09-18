# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import json
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(
        description="Generate reproducible native-video browser fixtures"
    )
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    pattern = (
        "nullsrc=size=256x128:rate=15:duration=4,"
        "geq=lum='if(lt(Y,H/2),80+20*sin(2*PI*N/30),160+20*cos(2*PI*N/30))':"
        "cb='if(lt(Y,H/2),90+20*cos(2*PI*N/30),180+20*sin(2*PI*N/30))':"
        "cr='if(lt(Y,H/2),180+20*sin(2*PI*N/30),90+20*cos(2*PI*N/30))'"
    )
    manifest = {}
    for name, pixel_format, profile, color_range in [
        ("pattern.webm", "yuv420p", "0", "tv"),
        ("pattern-full.webm", "yuv420p", "0", "pc"),
        ("pattern-10bit.webm", "yuv420p10le", "2", "tv"),
    ]:
        path = args.output / name
        subprocess.run(
            [
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                pattern,
                "-frames:v",
                "60",
                "-c:v",
                "libvpx-vp9",
                "-lossless",
                "1",
                "-g",
                "60",
                "-pix_fmt",
                pixel_format,
                "-profile:v",
                profile,
                "-color_range",
                color_range,
                "-colorspace",
                "bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                str(path),
            ],
            check=True,
        )
        manifest[name] = json.loads(
            subprocess.check_output(
                [
                    "ffprobe",
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-read_intervals",
                    "%+#1",
                    "-show_entries",
                    "frame=pix_fmt,color_range,color_space,color_primaries:format=duration",
                    "-of",
                    "json",
                    str(path),
                ],
                text=True,
            )
        )
    with tempfile.TemporaryDirectory(dir=args.output) as temporary:
        directory = Path(temporary)
        for index, size in enumerate(["256x128", "320x160"]):
            subprocess.run(
                [
                    "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
                    "-f", "lavfi", "-i", pattern.replace("256x128", size),
                    "-frames:v", "30", "-c:v", "libvpx-vp9", "-lossless", "1",
                    "-g", "30", "-pix_fmt", "yuv420p", "-color_range", "tv",
                    "-colorspace", "bt709", "-color_primaries", "bt709",
                    "-color_trc", "bt709", str(directory / f"{index}.webm"),
                ],
                check=True,
            )
        concat = directory / "inputs.txt"
        concat.write_text("file '0.webm'\nfile '1.webm'\n")
        resized = args.output / "pattern-resize.webm"
        subprocess.run(
            [
                "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
                "-f", "concat", "-i", str(concat), "-c", "copy", str(resized),
            ],
            check=True,
        )
        manifest[resized.name] = json.loads(subprocess.check_output(
            [
                "ffprobe", "-v", "error", "-select_streams", "v:0",
                "-show_entries", "frame=width,height:format=duration",
                "-of", "json", str(resized),
            ],
            text=True,
        ))
    (args.output / "fixtures.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
