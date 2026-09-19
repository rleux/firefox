# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import argparse
import hashlib
import json
import struct
import subprocess
import tempfile
from pathlib import Path


def probe(path, *, frame_sizes=False):
    result = json.loads(
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
                "frame=width,height,pix_fmt,color_range,color_space,"
                "color_primaries,color_transfer,chroma_location:format=duration",
                "-of",
                "json",
                str(path),
            ],
            text=True,
        )
    )
    result["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
    frame = result["frames"][0]
    if frame["pix_fmt"] in ("yuv420p10le", "yuv420p12le"):
        raw = subprocess.check_output([
            "ffmpeg",
            "-v",
            "error",
            "-i",
            str(path),
            "-frames:v",
            "1",
            "-pix_fmt",
            frame["pix_fmt"],
            "-f",
            "rawvideo",
            "-",
        ])
        samples = struct.unpack(f"<{len(raw) // 2}H", raw)
        result["decodedFirstFrame"] = {
            "sha256": hashlib.sha256(raw).hexdigest(),
            "uniqueCodes": sorted(set(samples)),
            "codesNotMultipleOfFour": sum(bool(sample & 3) for sample in samples),
        }
    if frame_sizes:
        frames = json.loads(
            subprocess.check_output(
                [
                    "ffprobe",
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "frame=width,height",
                    "-of",
                    "json",
                    str(path),
                ],
                text=True,
            )
        )["frames"]
        result["frameSizes"] = list(
            dict.fromkeys((frame["width"], frame["height"]) for frame in frames)
        )
    return result


def encode_p010(
    path,
    size,
    matrix,
    color_range,
    *,
    pixel_format="yuv420p10le",
    primaries=None,
    transfer=None,
    chroma_location="center",
    frames=60,
):
    if pixel_format == "yuv420p12le":
        low, high, cb, cr = 1281, 2879, 2053, 1029
    else:
        low, high, cb, cr = 321, 719, 513, 257
    pattern = (
        f"nullsrc=size={size}:rate=15:duration=4,format={pixel_format},"
        f"geq=lum='if(lt(Y,H/2),{low}+2*mod(N,4),{high}-2*mod(N,4))':"
        f"cb='{cb}+2*mod(N,4)':cr='{cr}+2*mod(N,4)'"
    )
    primaries = primaries or matrix
    transfer = transfer or primaries
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
            str(frames),
            "-c:v",
            "libvpx-vp9",
            "-lossless",
            "1",
            "-g",
            str(frames),
            "-row-mt",
            "0",
            "-threads",
            "1",
            "-pix_fmt",
            pixel_format,
            "-profile:v",
            "2",
            "-color_range",
            color_range,
            "-colorspace",
            matrix,
            "-color_primaries",
            primaries,
            "-color_trc",
            transfer,
            "-chroma_sample_location",
            chroma_location,
            str(path),
        ],
        check=True,
    )


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
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    pattern.replace("256x128", size),
                    "-frames:v",
                    "30",
                    "-c:v",
                    "libvpx-vp9",
                    "-lossless",
                    "1",
                    "-g",
                    "30",
                    "-pix_fmt",
                    "yuv420p",
                    "-color_range",
                    "tv",
                    "-colorspace",
                    "bt709",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    str(directory / f"{index}.webm"),
                ],
                check=True,
            )
        concat = directory / "inputs.txt"
        concat.write_text("file '0.webm'\nfile '1.webm'\n")
        resized = args.output / "pattern-resize.webm"
        subprocess.run(
            [
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "concat",
                "-i",
                str(concat),
                "-c",
                "copy",
                str(resized),
            ],
            check=True,
        )
        manifest[resized.name] = json.loads(
            subprocess.check_output(
                [
                    "ffprobe",
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "frame=width,height:format=duration",
                    "-of",
                    "json",
                    str(resized),
                ],
                text=True,
            )
        )

    p010_specs = [
        ("pattern-p010-bt601-limited.webm", "258x130", "smpte170m", "tv"),
        ("pattern-p010-bt601-full.webm", "258x130", "smpte170m", "pc"),
        ("pattern-p010-bt709-limited.webm", "258x130", "bt709", "tv"),
        ("pattern-p010-bt709-full.webm", "258x130", "bt709", "pc"),
        ("pattern-p010-odd-unsupported.webm", "255x127", "bt709", "tv"),
    ]
    for name, size, matrix, color_range in p010_specs:
        path = args.output / name
        encode_p010(path, size, matrix, color_range)
        manifest[name] = probe(path)

    left = args.output / "pattern-p010-left-unsupported.webm"
    encode_p010(left, "258x130", "bt709", "tv", chroma_location="left")
    manifest[left.name] = probe(left)

    hdr = args.output / "pattern-p010-hdr-pq-unsupported.webm"
    encode_p010(
        hdr,
        "258x130",
        "bt2020nc",
        "tv",
        primaries="bt2020",
        transfer="smpte2084",
    )
    manifest[hdr.name] = probe(hdr)

    twelve_bit = args.output / "pattern-12bit-unsupported.webm"
    encode_p010(twelve_bit, "258x130", "bt709", "tv", pixel_format="yuv420p12le")
    manifest[twelve_bit.name] = probe(twelve_bit)

    with tempfile.TemporaryDirectory(dir=args.output) as temporary:
        directory = Path(temporary)
        parts = []
        for index, size in enumerate(["256x128", "320x160"]):
            part = directory / f"p010-{index}.webm"
            encode_p010(part, size, "bt709", "tv", frames=30)
            parts.append(part)
        concat = directory / "p010-inputs.txt"
        concat.write_text("".join(f"file '{path.name}'\n" for path in parts))
        resized = args.output / "pattern-p010-resize.webm"
        subprocess.run(
            [
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "concat",
                "-i",
                str(concat),
                "-c",
                "copy",
                str(resized),
            ],
            check=True,
        )
        manifest[resized.name] = probe(resized, frame_sizes=True)
    (args.output / "fixtures.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
