# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import struct, zlib, json, sys
from pathlib import Path


def png(path):
    data = Path(path).read_bytes()
    offset = 8
    compressed = b""
    while offset < len(data):
        size = struct.unpack(">I", data[offset : offset + 4])[0]
        kind = data[offset + 4 : offset + 8]
        body = data[offset + 8 : offset + 8 + size]
        offset += 12 + size
        if kind == b"IHDR":
            width, height, bits, color, _, _, interlace = struct.unpack(
                ">IIBBBBB", body
            )
            assert bits == 8 and color == 6 and interlace == 0
        if kind == b"IDAT":
            compressed += body
    raw = zlib.decompress(compressed)
    rows = []
    stride = width * 4
    previous = [0] * stride
    for y in range(height):
        flag = raw[y * (stride + 1)]
        row = list(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        for x in range(stride):
            a = row[x - 4] if x >= 4 else 0
            b = previous[x]
            c = previous[x - 4] if x >= 4 else 0
            if flag == 1:
                row[x] = (row[x] + a) & 255
            elif flag == 2:
                row[x] = (row[x] + b) & 255
            elif flag == 3:
                row[x] = (row[x] + (a + b) // 2) & 255
            elif flag == 4:
                p = a + b - c
                pa = abs(p - a)
                pb = abs(p - b)
                pc = abs(p - c)
                row[x] = (
                    row[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)
                ) & 255
            else:
                assert flag == 0
        rows.append(row)
        previous = row
    return width, height, rows


if __name__ == "__main__":
    scene, reference, actual = sys.argv[1:]
    assert scene in ("alpha-depth", "odd-transform")
    width, height, expected = png(reference)
    actual_width, actual_height, rendered = png(actual)
    assert (width, height) == (actual_width, actual_height) == (257, 129)
    differences = 0
    maximum = 0
    for y in range(height):
        for x in range(width):
            linear = scene == "odd-transform" and 132 <= x <= 237 and 16 <= y <= 90
            for channel in range(4):
                difference = abs(
                    expected[y][x * 4 + channel] - rendered[y][x * 4 + channel]
                )
                allowed = 2 if linear and channel != 3 else 0
                assert difference <= allowed, (x, y, channel, difference, allowed)
                differences += difference != 0
                maximum = max(maximum, difference)
    print(
        json.dumps(
            dict(
                scene=scene,
                differing_channels=differences,
                max_difference=maximum,
                exact_geometry_nearest_alpha=True,
                linear_precision_bound=2,
            )
        )
    )
