# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

"""Task-only localhost pixel sampler; run on the actual browser's X11 host.
No images/files are written. Samples the displayed root framebuffer, not canvas
snapshot APIs. Keep the small test canvas visible and unobscured.
"""

import argparse
import ctypes as C
import ctypes.util
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import hmac
import os
from urllib.parse import parse_qs, urlparse


class XImage(C.Structure):
    _fields_ = (
        [(n, C.c_int) for n in ("width", "height", "xoffset", "format")]
        + [("data", C.c_void_p)]
        + [
            (n, C.c_int)
            for n in (
                "byte_order",
                "bitmap_unit",
                "bitmap_bit_order",
                "bitmap_pad",
                "depth",
                "bytes_per_line",
                "bits_per_pixel",
            )
        ]
        + [(n, C.c_ulong) for n in ("red_mask", "green_mask", "blue_mask")]
    )


p = argparse.ArgumentParser()
p.add_argument("--port", type=int, default=2830)
a = p.parse_args()
token = os.environ.get("WR_CPU_CANVAS_PIXEL_TOKEN", "")
assert len(token) >= 32, (
    "Set a fresh high-entropy WR_CPU_CANVAS_PIXEL_TOKEN (at least 32 ASCII characters)"
)
assert token.isascii()
x = C.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
x.XOpenDisplay.argtypes = [C.c_char_p]
x.XOpenDisplay.restype = C.c_void_p
x.XDefaultRootWindow.argtypes = [C.c_void_p]
x.XDefaultRootWindow.restype = C.c_ulong
x.XGetGeometry.argtypes = [
    C.c_void_p,
    C.c_ulong,
    C.POINTER(C.c_ulong),
    C.POINTER(C.c_int),
    C.POINTER(C.c_int),
    C.POINTER(C.c_uint),
    C.POINTER(C.c_uint),
    C.POINTER(C.c_uint),
    C.POINTER(C.c_uint),
]
x.XGetGeometry.restype = C.c_int
x.XGetImage.argtypes = [
    C.c_void_p,
    C.c_ulong,
    C.c_int,
    C.c_int,
    C.c_uint,
    C.c_uint,
    C.c_ulong,
    C.c_int,
]
x.XGetImage.restype = C.POINTER(XImage)
x.XGetPixel.argtypes = [C.POINTER(XImage), C.c_int, C.c_int]
x.XGetPixel.restype = C.c_ulong
x.XDestroyImage.argtypes = [C.POINTER(XImage)]
x.XCloseDisplay.argtypes = [C.c_void_p]
display = x.XOpenDisplay(None)
assert display, "Cannot open browser host DISPLAY"
root = x.XDefaultRootWindow(display)


def channel(pixel, mask):
    assert mask
    shift = (mask & -mask).bit_length() - 1
    return round(((pixel & mask) >> shift) * 255 / (mask >> shift))


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if urlparse(self.path).path == "/empty.html":
            body = b"<!doctype html><meta charset=utf-8><title>CPU WebGPU canvas</title><body></body>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        try:
            query = parse_qs(urlparse(self.path).query)
            supplied = query.get("token", [""])
            if (
                len(supplied) != 1
                or not supplied[0].isascii()
                or not hmac.compare_digest(token, supplied[0])
            ):
                self.send_error(403, "Invalid sampler credential")
                return
            px, py = int(query["x"][0]), int(query["y"][0])
            geometry_root, gx, gy = C.c_ulong(), C.c_int(), C.c_int()
            width, height, border, depth = (C.c_uint() for _ in range(4))
            assert x.XGetGeometry(
                display,
                root,
                C.byref(geometry_root),
                C.byref(gx),
                C.byref(gy),
                C.byref(width),
                C.byref(height),
                C.byref(border),
                C.byref(depth),
            ), "Cannot query X11 root geometry"
            assert 0 <= px < width.value and 0 <= py < height.value, (
                "Sample outside root bounds"
            )
            image = x.XGetImage(display, root, px, py, 1, 1, C.c_ulong(-1).value, 2)
            assert image, "XGetImage failed"
            try:
                pixel = x.XGetPixel(image, 0, 0)
                rgb = [
                    channel(pixel, getattr(image.contents, name))
                    for name in ("red_mask", "green_mask", "blue_mask")
                ]
            finally:
                x.XDestroyImage(image)
            body = json.dumps({"rgb": rgb}).encode()
            self.send_response(200)
        except Exception as error:
            body = json.dumps({"error": str(error)}).encode()
            self.send_response(400)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


try:
    print(f"X11 root pixel sampler on 127.0.0.1:{a.port}", flush=True)
    HTTPServer(("127.0.0.1", a.port), Handler).serve_forever()
finally:
    x.XCloseDisplay(display)
