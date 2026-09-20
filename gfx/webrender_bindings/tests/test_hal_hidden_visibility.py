# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import base64
import ctypes
import http.server
import json
import os
import threading
import time
import urllib.parse
import uuid
from collections import defaultdict
from pathlib import Path

from marionette_harness import MarionetteTestCase

PREFIX = "WR HAL render metrics: "
SCREEN_COUNTERS = [
    "fullCompositions",
    "partialCompositions",
    "acquires",
    "presents",
    "fullPresentUpdates",
    "partialPresentUpdates",
    "unchangedPresentUpdates",
]


class XWindowAttributes(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_int),
        ("y", ctypes.c_int),
        ("width", ctypes.c_int),
        ("height", ctypes.c_int),
        ("border_width", ctypes.c_int),
        ("depth", ctypes.c_int),
        ("visual", ctypes.c_void_p),
        ("root", ctypes.c_ulong),
        ("class_", ctypes.c_int),
        ("bit_gravity", ctypes.c_int),
        ("win_gravity", ctypes.c_int),
        ("backing_store", ctypes.c_int),
        ("backing_planes", ctypes.c_ulong),
        ("backing_pixel", ctypes.c_ulong),
        ("save_under", ctypes.c_int),
        ("colormap", ctypes.c_ulong),
        ("map_installed", ctypes.c_int),
        ("map_state", ctypes.c_int),
        ("all_event_masks", ctypes.c_long),
        ("your_event_mask", ctypes.c_long),
        ("do_not_propagate_mask", ctypes.c_long),
        ("override_redirect", ctypes.c_int),
        ("screen", ctypes.c_void_p),
    ]


class ClientData(ctypes.Union):
    _fields_ = [
        ("bytes", ctypes.c_char * 20),
        ("shorts", ctypes.c_short * 10),
        ("longs", ctypes.c_long * 5),
    ]


class ClientMessageEvent(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", ctypes.c_ulong),
        ("message_type", ctypes.c_ulong),
        ("format", ctypes.c_int),
        ("data", ClientData),
    ]


class XEvent(ctypes.Union):
    _fields_ = [("client", ClientMessageEvent), ("padding", ctypes.c_long * 24)]


class X11:
    def __init__(self):
        self.lib = ctypes.CDLL("libX11.so.6")
        pointer = ctypes.c_void_p
        window = ctypes.c_ulong
        self.lib.XOpenDisplay.restype = pointer
        self.lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.display = self.lib.XOpenDisplay(os.environ["DISPLAY"].encode())
        if not self.display:
            raise RuntimeError("XOpenDisplay failed")
        self.lib.XDefaultRootWindow.restype = window
        self.lib.XDefaultRootWindow.argtypes = [pointer]
        self.lib.XDefaultScreen.restype = ctypes.c_int
        self.lib.XDefaultScreen.argtypes = [pointer]
        self.lib.XQueryTree.argtypes = [
            pointer,
            window,
            ctypes.POINTER(window),
            ctypes.POINTER(window),
            ctypes.POINTER(ctypes.POINTER(window)),
            ctypes.POINTER(ctypes.c_uint),
        ]
        self.lib.XFetchName.argtypes = [
            pointer,
            window,
            ctypes.POINTER(ctypes.c_char_p),
        ]
        self.lib.XInternAtom.restype = ctypes.c_ulong
        self.lib.XInternAtom.argtypes = [pointer, ctypes.c_char_p, ctypes.c_int]
        self.lib.XGetWindowProperty.argtypes = [
            pointer,
            window,
            ctypes.c_ulong,
            ctypes.c_long,
            ctypes.c_long,
            ctypes.c_int,
            ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte)),
        ]
        self.lib.XGetWindowAttributes.argtypes = [
            pointer,
            window,
            ctypes.POINTER(XWindowAttributes),
        ]
        self.lib.XFree.argtypes = [pointer]
        self.lib.XUnmapWindow.argtypes = [pointer, window]
        self.lib.XMapRaised.argtypes = [pointer, window]
        self.lib.XResizeWindow.argtypes = [
            pointer,
            window,
            ctypes.c_uint,
            ctypes.c_uint,
        ]
        self.lib.XIconifyWindow.argtypes = [pointer, window, ctypes.c_int]
        self.lib.XSync.argtypes = [pointer, ctypes.c_int]
        self.lib.XSendEvent.argtypes = [
            pointer,
            window,
            ctypes.c_int,
            ctypes.c_long,
            ctypes.c_void_p,
        ]
        self.lib.XGetImage.restype = pointer
        self.lib.XGetImage.argtypes = [
            pointer,
            window,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_ulong,
            ctypes.c_int,
        ]
        self.lib.XGetPixel.restype = ctypes.c_ulong
        self.lib.XGetPixel.argtypes = [pointer, ctypes.c_int, ctypes.c_int]
        self.lib.XDestroyImage.argtypes = [pointer]
        self.lib.XCloseDisplay.argtypes = [pointer]
        self.root = self.lib.XDefaultRootWindow(self.display)
        self.net_wm_name = self.lib.XInternAtom(self.display, b"_NET_WM_NAME", 0)
        self.utf8_string = self.lib.XInternAtom(self.display, b"UTF8_STRING", 0)
        self.wm_class = self.lib.XInternAtom(self.display, b"WM_CLASS", 0)
        self.wm_state = self.lib.XInternAtom(self.display, b"WM_STATE", 0)
        self.net_wm_state = self.lib.XInternAtom(self.display, b"_NET_WM_STATE", 0)
        self.net_wm_state_hidden = self.lib.XInternAtom(
            self.display, b"_NET_WM_STATE_HIDDEN", 0
        )
        self.net_client_list = self.lib.XInternAtom(
            self.display, b"_NET_CLIENT_LIST", 0
        )
        self.net_active_window = self.lib.XInternAtom(
            self.display, b"_NET_ACTIVE_WINDOW", 0
        )
        self.net_number_of_desktops = self.lib.XInternAtom(
            self.display, b"_NET_NUMBER_OF_DESKTOPS", 0
        )
        self.net_current_desktop = self.lib.XInternAtom(
            self.display, b"_NET_CURRENT_DESKTOP", 0
        )
        self.net_wm_desktop = self.lib.XInternAtom(self.display, b"_NET_WM_DESKTOP", 0)

    def children(self, window):
        root = ctypes.c_ulong()
        parent = ctypes.c_ulong()
        children = ctypes.POINTER(ctypes.c_ulong)()
        count = ctypes.c_uint()
        if not self.lib.XQueryTree(
            self.display,
            window,
            ctypes.byref(root),
            ctypes.byref(parent),
            ctypes.byref(children),
            ctypes.byref(count),
        ):
            return parent.value, []
        values = [children[index] for index in range(count.value)]
        if children:
            self.lib.XFree(children)
        return parent.value, values

    def title(self, window):
        actual_type = ctypes.c_ulong()
        actual_format = ctypes.c_int()
        count = ctypes.c_ulong()
        remaining = ctypes.c_ulong()
        data = ctypes.POINTER(ctypes.c_ubyte)()
        status = self.lib.XGetWindowProperty(
            self.display,
            window,
            self.net_wm_name,
            0,
            1024,
            0,
            self.utf8_string,
            ctypes.byref(actual_type),
            ctypes.byref(actual_format),
            ctypes.byref(count),
            ctypes.byref(remaining),
            ctypes.byref(data),
        )
        if status == 0 and data and actual_format.value == 8:
            try:
                return ctypes.string_at(data, count.value).decode(errors="replace")
            finally:
                self.lib.XFree(data)
        value = ctypes.c_char_p()
        if not self.lib.XFetchName(self.display, window, ctypes.byref(value)):
            return ""
        try:
            return value.value.decode(errors="replace") if value.value else ""
        finally:
            if value:
                self.lib.XFree(value)

    def property(self, window, atom):
        actual_type = ctypes.c_ulong()
        actual_format = ctypes.c_int()
        count = ctypes.c_ulong()
        remaining = ctypes.c_ulong()
        data = ctypes.POINTER(ctypes.c_ubyte)()
        status = self.lib.XGetWindowProperty(
            self.display,
            window,
            atom,
            0,
            1024,
            0,
            0,
            ctypes.byref(actual_type),
            ctypes.byref(actual_format),
            ctypes.byref(count),
            ctypes.byref(remaining),
            ctypes.byref(data),
        )
        if status != 0 or not data:
            return b""
        try:
            width = actual_format.value // 8
            return ctypes.string_at(data, count.value * width) if width else b""
        finally:
            self.lib.XFree(data)

    def atom_values(self, window, atom):
        actual_type = ctypes.c_ulong()
        actual_format = ctypes.c_int()
        count = ctypes.c_ulong()
        remaining = ctypes.c_ulong()
        data = ctypes.POINTER(ctypes.c_ubyte)()
        status = self.lib.XGetWindowProperty(
            self.display,
            window,
            atom,
            0,
            64,
            0,
            0,
            ctypes.byref(actual_type),
            ctypes.byref(actual_format),
            ctypes.byref(count),
            ctypes.byref(remaining),
            ctypes.byref(data),
        )
        if status != 0 or not data or actual_format.value != 32:
            return []
        try:
            values = ctypes.cast(data, ctypes.POINTER(ctypes.c_ulong))
            return [values[index] for index in range(count.value)]
        finally:
            self.lib.XFree(data)

    def describe(self, window):
        attributes = self.attributes(window)
        return {
            "window": window,
            "parent": self.parent(window),
            "ancestors": self.ancestors(window),
            "mapState": attributes.map_state if attributes else None,
            "title": self.title(window),
            "wmClass": self.property(window, self.wm_class).decode(errors="replace"),
            "wmState": self.atom_values(window, self.wm_state),
            "netWmState": self.atom_values(window, self.net_wm_state),
        }

    def find(self, text):
        for window in self.atom_values(self.root, self.net_client_list):
            parent = self.parent(window)
            if (
                text in self.title(window)
                and self.atom_values(window, self.wm_state)
                and parent not in [0, self.root]
            ):
                return window
        return None

    def ancestors(self, window):
        result = []
        parent = self.parent(window)
        while parent not in [0, self.root] and parent not in result:
            result.append(parent)
            parent = self.parent(parent)
        return result

    def attributes(self, window):
        attributes = XWindowAttributes()
        if not self.lib.XGetWindowAttributes(
            self.display, window, ctypes.byref(attributes)
        ):
            return None
        return attributes

    def parent(self, window):
        return self.children(window)[0]

    def unmap(self, window):
        self.lib.XUnmapWindow(self.display, window)
        self.lib.XSync(self.display, 0)

    def resize(self, window, width, height):
        self.lib.XResizeWindow(self.display, window, width, height)
        self.lib.XSync(self.display, 0)

    def restore(self, frame, client):
        ancestors = self.ancestors(client)
        if frame not in ancestors and frame not in [0, self.root]:
            ancestors.insert(0, frame)
        for ancestor in reversed(ancestors):
            self.lib.XMapRaised(self.display, ancestor)
        self.lib.XMapRaised(self.display, client)
        self.client_message(client, self.net_active_window, 1)
        self.lib.XSync(self.display, 0)

    def client_message(self, window, message, first, second=0):
        event = XEvent()
        event.client = ClientMessageEvent(
            type=33,
            display=self.display,
            window=window,
            message_type=message,
            format=32,
        )
        event.client.data.longs[0] = first
        event.client.data.longs[1] = second
        self.lib.XSendEvent(
            self.display,
            self.root,
            0,
            (1 << 20) | (1 << 19),
            ctypes.byref(event),
        )

    def desktop(self, window):
        values = self.atom_values(window, self.net_wm_desktop)
        return values[0] if values else None

    def current_desktop(self):
        values = self.atom_values(self.root, self.net_current_desktop)
        return values[0] if values else None

    def desktop_count(self):
        values = self.atom_values(self.root, self.net_number_of_desktops)
        return values[0] if values else 0

    def ensure_desktops(self, count):
        if self.desktop_count() < count:
            self.client_message(self.root, self.net_number_of_desktops, count)
            self.lib.XSync(self.display, 0)

    def move_to_desktop(self, client, desktop):
        self.client_message(client, self.net_wm_desktop, desktop, 2)
        self.lib.XSync(self.display, 0)

    def iconify(self, client):
        if not self.lib.XIconifyWindow(
            self.display, client, self.lib.XDefaultScreen(self.display)
        ):
            raise RuntimeError("XIconifyWindow failed")
        self.lib.XSync(self.display, 0)

    def deiconify(self, frame, client):
        self.iconify(client)
        self.restore(frame, client)

    def pixel(self, window):
        attributes = self.attributes(window)
        if not attributes or attributes.width <= 0 or attributes.height <= 0:
            return None
        image = self.lib.XGetImage(
            self.display,
            window,
            attributes.width // 2,
            attributes.height * 3 // 4,
            1,
            1,
            ctypes.c_ulong(-1).value,
            2,
        )
        if not image:
            return None
        try:
            return self.lib.XGetPixel(image, 0, 0) & 0xFFFFFF
        finally:
            self.lib.XDestroyImage(image)

    def close(self):
        self.lib.XCloseDisplay(self.display)


def metric_records(path):
    result = []
    if not path.exists():
        return result
    for line in path.read_text(errors="replace").splitlines():
        if PREFIX not in line:
            continue
        try:
            value = json.loads(line.split(PREFIX, 1)[1])
        except json.JSONDecodeError:
            continue
        if value.get("version") == 1:
            result.append(value)
    return result


def latest_scopes(path):
    values = defaultdict(list)
    for value in metric_records(path):
        if value["rendererId"] > 0:
            key = (value["pid"], value["deviceId"], value["rendererId"])
            values[key].append(value)
    return {
        key: max(samples, key=lambda sample: sample["sequence"])
        for key, samples in values.items()
    }


def counter_delta(before, after):
    return {
        name: after["counters"].get(name, 0) - before["counters"].get(name, 0)
        for name in after["counters"].keys() | before["counters"].keys()
    }


class TestHalHiddenVisibility(MarionetteTestCase):
    def setUp(self):
        super().setUp()
        self.output = Path(os.environ["WR_WEBGPU_OUTPUT"])
        self.report = {"passed": False}
        self.page = Path(__file__).with_name("hal_hidden_visibility.html").read_bytes()

        page = self.page

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(page)))
                self.end_headers()
                self.wfile.write(page)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.x11 = X11()
        self.marionette.set_context("content")
        self.marionette.set_window_rect(x=40, y=40, width=900, height=700)
        self.main_handle = self.marionette.current_window_handle
        nonce = uuid.uuid4().hex
        self.main_title = f"HAL-main-{nonce}"
        self.other_title = f"HAL-other-{nonce}"
        self.marionette.navigate(self.url(self.main_title))
        self.call("ready")
        self.other_handle = self.marionette.open(type="window", focus=True)["handle"]
        self.marionette.switch_to_window(self.other_handle)
        self.marionette.set_window_rect(x=650, y=80, width=600, height=600)
        self.marionette.navigate(self.url(self.other_title))
        self.call("ready")
        self.marionette.switch_to_window(self.main_handle)
        self.main_window = self.wait_native(self.main_title)
        self.other_window = self.wait_native(self.other_title)
        self.main_frame = self.x11.parent(self.main_window)
        self.other_frame = self.x11.parent(self.other_window)
        for frame, client in [
            (self.main_frame, self.main_window),
            (self.other_frame, self.other_window),
        ]:
            self.assertNotEqual(frame, 0)
            self.assertNotEqual(frame, self.x11.root)
            self.assertNotEqual(frame, client)
        self.assertNotEqual(self.main_frame, self.other_frame)
        self.x11.ensure_desktops(2)
        self.wait_for_condition(lambda _: self.x11.desktop_count() >= 2, timeout=10)
        self.original_desktop = self.x11.current_desktop()
        self.assertIsNotNone(self.original_desktop)
        self.hidden_desktop = (self.original_desktop + 1) % self.x11.desktop_count()
        self.assertEqual(self.x11.desktop(self.main_window), self.original_desktop)
        self.assertEqual(self.x11.desktop(self.other_window), self.original_desktop)
        x11_evidence = {
            "main": self.x11.describe(self.main_window),
            "other": self.x11.describe(self.other_window),
            "desktopCount": self.x11.desktop_count(),
            "originalDesktop": self.original_desktop,
            "hiddenDesktop": self.hidden_desktop,
        }
        self.report["x11"] = x11_evidence
        print(
            "HAL_HIDDEN_X11",
            json.dumps(x11_evidence, sort_keys=True),
            flush=True,
        )

    def tearDown(self):
        try:
            if hasattr(self, "main_frame"):
                if hasattr(self, "original_desktop"):
                    self.x11.move_to_desktop(self.main_window, self.original_desktop)
                self.x11.restore(self.main_frame, self.main_window)
            (self.output / "report.json").write_text(
                json.dumps(self.report, indent=2) + "\n"
            )
            self.x11.close()
            self.server.shutdown()
            self.server.server_close()
            self.thread.join()
        finally:
            super().tearDown()

    def url(self, title):
        return (
            f"http://127.0.0.1:{self.server.server_port}/?title="
            + urllib.parse.quote(title)
        )

    def call(self, method, *args):
        result = self.marionette.execute_async_script(
            """
            const done = arguments[arguments.length - 1];
            const [method, args] = arguments;
            Promise.resolve(window.hiddenVisibility[method](...args)).then(done,
              error => done({error: String(error)}));
            """,
            script_args=[method, list(args)],
            sandbox=None,
            script_timeout=15000,
        )
        self.assertFalse(isinstance(result, dict) and "error" in result, result)
        return result

    def state(self):
        return self.marionette.execute_script(
            "return window.hiddenVisibility.state();", sandbox=None
        )

    def wait_native(self, title):
        result = None

        def find(_):
            nonlocal result
            result = self.x11.find(title)
            return result is not None

        self.wait_for_condition(find, timeout=10)
        return result

    def wait_map_state(self, window, expected):
        self.wait_for_condition(
            lambda _: (
                (attributes := self.x11.attributes(window)) is not None
                and attributes.map_state == expected
            ),
            timeout=10,
        )

    def wait_hidden_map_state(self, window):
        result = None

        def hidden(_):
            nonlocal result
            attributes = self.x11.attributes(window)
            result = attributes.map_state if attributes else None
            return result in [0, 1]

        self.wait_for_condition(hidden, timeout=10)
        return result

    def wait_native_hidden(self, window):
        result = None

        def hidden(_):
            nonlocal result
            result = self.x11.describe(window)
            return (
                result["mapState"] in [0, 1]
                or self.x11.net_wm_state_hidden in result["netWmState"]
            )

        self.wait_for_condition(hidden, timeout=10)
        return result

    def snapshot(self, previous=None, min_fresh=2, required_scope=None):
        log = self.output / "gecko.log"
        result = None

        def fresh(_):
            nonlocal result
            result = latest_scopes(log)
            if len(result) < 2:
                return False
            if previous is None:
                return True
            common = previous.keys() & result.keys()
            if required_scope is not None and (
                required_scope not in common
                or result[required_scope]["sequence"]
                <= previous[required_scope]["sequence"]
            ):
                return False
            return (
                sum(
                    result[key]["sequence"] > previous[key]["sequence"]
                    for key in common
                )
                >= min_fresh
            )

        self.wait_for_condition(fresh, timeout=15)
        return result

    def settle_snapshot(self, previous=None, min_fresh=2, required_scope=None):
        time.sleep(1.2)
        return self.snapshot(previous, min_fresh, required_scope)

    def hidden_scope(self, before, after, require_skip):
        deltas = {
            key: counter_delta(before[key], after[key])
            for key in before.keys() & after.keys()
        }
        candidates = []
        for key, delta in deltas.items():
            if (
                all(delta[name] == 0 for name in SCREEN_COUNTERS)
                and delta["executions"] == delta["offscreenExecutions"]
            ):
                if not require_skip or delta["hiddenSkips"] > 0:
                    candidates.append(key)
        self.assertTrue(candidates, deltas)
        scope = max(candidates, key=lambda key: deltas[key]["hiddenSkips"])
        visible = [
            key
            for key, delta in deltas.items()
            if key != scope and delta["presents"] > 0
        ]
        self.assertTrue(visible, deltas)
        return scope, deltas

    def scope_with_hidden_skips(self, before, after):
        deltas = {
            key: counter_delta(before[key], after[key])
            for key in before.keys() & after.keys()
        }
        candidates = [key for key, delta in deltas.items() if delta["hiddenSkips"] > 0]
        self.assertTrue(candidates, deltas)
        return max(candidates, key=lambda key: deltas[key]["hiddenSkips"]), deltas

    def screenshot_hidden(self, name, expected, full=False, strict=True):
        shot = self.marionette.screenshot(full=full)
        data = base64.b64decode(shot)
        self.assertGreater(len(data), 100)
        (self.output / f"{name}.png").write_bytes(data)
        self.marionette.switch_to_window(self.other_handle)
        try:
            pixel = self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                const image = new Image();
                image.onload = () => {
                  const canvas = document.createElement("canvas");
                  canvas.width = image.width;
                  canvas.height = image.height;
                  const context = canvas.getContext("2d");
                  context.drawImage(image, 0, 0);
                  done({width: image.width, height: image.height,
                    pixel: [...context.getImageData(
                      Math.floor(image.width / 2), Math.floor(image.height / 2),
                      1, 1).data]});
                };
                image.onerror = () => done(null);
                image.src = "data:image/png;base64," + arguments[0];
                """,
                script_args=[shot],
            )
        finally:
            self.marionette.switch_to_window(self.main_handle)
        self.assertIsNotNone(pixel)
        self.assertGreater(pixel["width"], 0)
        self.assertGreater(pixel["height"], 0)
        expected_rgba = [
            expected >> 16,
            expected >> 8 & 0xFF,
            expected & 0xFF,
            255,
        ]
        pixel["expected"] = expected_rgba
        pixel["matches"] = pixel["pixel"] == expected_rgba
        if strict:
            self.assertTrue(pixel["matches"], pixel)
        return pixel

    def expected_pixel(self, color):
        values = [int(value.strip()) for value in color[4:-1].split(",")]
        return values[0] << 16 | values[1] << 8 | values[2]

    def restore_and_check(self, paused, strict=True):
        self.x11.move_to_desktop(self.main_window, self.original_desktop)
        self.wait_for_condition(
            lambda _: self.x11.desktop(self.main_window) == self.original_desktop,
            timeout=10,
        )
        attributes = self.x11.attributes(self.main_window)
        if not attributes or attributes.map_state != 2:
            self.x11.deiconify(self.main_frame, self.main_window)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            current = self.x11.find(self.main_title)
            if current is not None:
                self.main_window = current
                self.main_frame = self.x11.parent(current)
                attributes = self.x11.attributes(self.main_window)
                if (
                    self.main_frame not in [0, self.x11.root, self.main_window]
                    and self.main_frame != self.other_frame
                    and attributes
                    and attributes.map_state == 2
                ):
                    break
            time.sleep(0.05)
        else:
            print(
                "HAL_HIDDEN_RESTORE_FAILURE",
                json.dumps(self.x11.describe(self.main_window), sort_keys=True),
                flush=True,
            )
            self.fail("Main X11 window did not become viewable after restore")
        expected = self.expected_pixel(paused["color"])
        if strict:
            self.wait_for_condition(
                lambda _: self.x11.pixel(self.main_window) == expected
            )
            return expected
        observed = self.x11.pixel(self.main_window)
        deadline = time.monotonic() + 10
        while observed != expected and time.monotonic() < deadline:
            time.sleep(0.05)
            observed = self.x11.pixel(self.main_window)
        return {
            "expected": expected,
            "observed": observed,
            "matches": observed == expected,
        }

    def visible_pixel(self):
        result = None

        def sample(_):
            nonlocal result
            result = self.x11.pixel(self.main_window)
            return result in [0xDC283C, 0x14B450]

        self.wait_for_condition(sample, timeout=10)
        return result

    def pause_opposite(self, previous):
        paused = self.call("pause")
        for _ in range(3):
            if self.expected_pixel(paused["color"]) != previous:
                return paused
            self.call("resume")
            time.sleep(0.45)
            paused = self.call("pause")
        self.fail("Hidden CSS animation did not advance to the opposite color")

    def backend_info(self):
        with self.marionette.using_context("chrome"):
            return self.marionette.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                window.windowUtils.getWebRenderBackendInfo().then(
                  info => done(JSON.parse(info)), error => done({error: String(error)}));
                """
            )

    def initial_scopes(self):
        baseline = self.settle_snapshot()
        main_scope = max(
            baseline,
            key=lambda key: baseline[key]["gauges"]["retainedOutputBytes"],
        )
        return baseline, main_scope

    def other_state(self):
        self.marionette.switch_to_window(self.other_handle)
        try:
            return self.state()
        finally:
            self.marionette.switch_to_window(self.main_handle)

    def finish_other(self, start):
        self.marionette.switch_to_window(self.other_handle)
        try:
            end = self.call("pause")
        finally:
            self.marionette.switch_to_window(self.main_handle)
        self.assertGreater(end["time"], start["time"])
        pixel = self.expected_pixel(end["color"])
        self.wait_for_condition(
            lambda _: self.x11.pixel(self.other_window) == pixel,
            timeout=10,
        )
        return {"pixel": pixel, "progress": [start["time"], end["time"]]}

    def validate_log(self):
        text = (self.output / "gecko.log").read_text(errors="replace")
        for failure in ["Validation Error", "VUID-", "DeviceReset", "panicked"]:
            self.assertNotIn(failure, text)

    def run_iconify(self, backend):
        baseline, main_scope = self.initial_scopes()
        other_start = self.other_state()
        print("HAL_HIDDEN_PHASE iconify start", flush=True)
        self.x11.iconify(self.main_window)
        map_state = self.wait_hidden_map_state(self.main_window)
        entry = self.settle_snapshot(baseline, 1)
        hidden = self.settle_snapshot(entry, 1)
        idle_delta = counter_delta(entry[main_scope], hidden[main_scope])
        for name in SCREEN_COUNTERS:
            self.assertEqual(idle_delta[name], 0, idle_delta)
        self.assertEqual(idle_delta["executions"], idle_delta["offscreenExecutions"])
        paused = self.call("pause")
        before = self.settle_snapshot(hidden, 1)
        expected = self.expected_pixel(paused["color"])
        geometry = self.x11.describe(self.main_window)
        screenshot = self.screenshot_hidden(
            "iconified-readback", expected, full=True, strict=False
        )
        time.sleep(1.1)
        self.screenshot_hidden("iconified-boundary", expected, full=True, strict=False)
        after = self.snapshot(before, 1)
        main_progress = after[main_scope]["sequence"] > before[main_scope]["sequence"]
        readback = counter_delta(before[main_scope], after[main_scope])
        for name in [
            "acquires",
            "presents",
            "fullPresentUpdates",
            "partialPresentUpdates",
            "unchangedPresentUpdates",
        ]:
            self.assertEqual(readback[name], 0, readback)
        force_visible = os.environ.get("WR_HAL_FORCE_VISIBLE") == "1"
        if main_progress:
            self.assertGreater(readback["readbacks"], 0, readback)
            self.assertGreater(readback["executions"], 0, readback)
            self.assertGreater(
                readback["fullCompositions"] + readback["partialCompositions"],
                0,
            )
        restore_pixel = self.restore_and_check(paused, strict=not force_visible)
        other = self.finish_other(other_start)
        self.validate_log()
        self.report.update(
            passed=True,
            phase="iconify",
            backend=backend,
            mainScope=main_scope,
            iconifyMapState=map_state,
            iconifyDelta=idle_delta,
            iconifyReadbackDelta=readback,
            iconifyMainProgress=main_progress,
            iconifyScreenshot=screenshot,
            iconifyCaptureX11=geometry,
            iconifyRestorePixel=restore_pixel,
            other=other,
            forceVisible=force_visible,
        )

    def test_hidden_native_guard(self):
        backend = self.backend_info()
        self.assertEqual(backend["backend"], "Vulkan (wgpu-hal)")
        self.assertEqual(backend["process"], os.environ["WR_WEBGPU_PROCESS"])
        self.report["backend"] = backend
        phase = os.environ.get("WR_HAL_HIDDEN_BROWSER_PHASE", "workspace")
        if phase == "iconify":
            self.run_iconify(backend)
            return
        baseline, _ = self.initial_scopes()
        other_start = self.other_state()

        direct_visible_pixel = self.visible_pixel()
        print("HAL_HIDDEN_PHASE workspace-hide start", flush=True)
        self.x11.move_to_desktop(self.main_window, self.hidden_desktop)
        self.wait_for_condition(
            lambda _: self.x11.desktop(self.main_window) == self.hidden_desktop,
            timeout=10,
        )
        workspace_hidden_state = self.wait_native_hidden(self.main_window)
        hidden_entry = self.settle_snapshot(baseline)
        entry_scope, entry_deltas = self.scope_with_hidden_skips(baseline, hidden_entry)
        hidden = self.settle_snapshot(hidden_entry)
        self.wait_native_hidden(self.main_window)
        main_scope, direct_deltas = self.hidden_scope(hidden_entry, hidden, True)
        self.assertEqual(main_scope, entry_scope)
        other_scope = max(
            (key for key in direct_deltas if key != main_scope),
            key=lambda key: direct_deltas[key]["presents"],
        )
        self.assertGreater(direct_deltas[other_scope]["presents"], 0)
        paused = self.pause_opposite(direct_visible_pixel)
        before_readback = self.settle_snapshot(hidden, 1)
        self.wait_native_hidden(self.main_window)
        print("HAL_HIDDEN_PHASE workspace-hide readback", flush=True)
        direct_expected = self.expected_pixel(paused["color"])
        direct_capture_x11 = self.x11.describe(self.main_window)
        direct_screenshot = self.screenshot_hidden(
            "direct-hidden-readback", direct_expected
        )
        time.sleep(1.1)
        self.screenshot_hidden("direct-hidden-boundary", direct_expected)
        self.wait_native_hidden(self.main_window)
        after_readback = self.snapshot(before_readback, required_scope=main_scope)
        readback_delta = counter_delta(
            before_readback[main_scope], after_readback[main_scope]
        )
        for name in [
            "acquires",
            "presents",
            "fullPresentUpdates",
            "partialPresentUpdates",
            "unchangedPresentUpdates",
        ]:
            self.assertEqual(readback_delta[name], 0, readback_delta)
        self.assertGreater(readback_delta["readbacks"], 0, readback_delta)
        self.assertGreater(readback_delta["executions"], 0, readback_delta)
        self.assertGreater(
            readback_delta["fullCompositions"] + readback_delta["partialCompositions"],
            0,
            readback_delta,
        )
        self.call("resume")
        time.sleep(1.2)
        before_resize = self.snapshot(after_readback, 1, required_scope=main_scope)
        self.x11.resize(self.main_window, 760, 620)
        self.wait_for_condition(
            lambda _: (
                (attributes := self.x11.attributes(self.main_window)) is not None
                and attributes.width == 760
                and attributes.height == 620
            ),
            timeout=10,
        )
        self.wait_native_hidden(self.main_window)
        time.sleep(1.2)
        after_resize = self.snapshot(before_resize, 1, required_scope=main_scope)
        resize_delta = counter_delta(
            before_resize[main_scope], after_resize[main_scope]
        )
        for name in SCREEN_COUNTERS:
            self.assertEqual(resize_delta[name], 0, resize_delta)
        self.assertEqual(
            resize_delta["executions"],
            resize_delta["offscreenExecutions"],
            resize_delta,
        )
        self.assertGreater(resize_delta["hiddenSkips"], 0, resize_delta)
        resize_x11 = self.x11.describe(self.main_window)
        restore_state = self.pause_opposite(direct_expected)
        time.sleep(0.2)
        direct_pixel = self.restore_and_check(restore_state)
        print("HAL_HIDDEN_PHASE workspace-hide end", flush=True)

        rapid_cycles = []
        cycle_baseline = after_resize
        for index in range(2):
            self.call("resume")
            cycle_baseline = self.settle_snapshot(
                cycle_baseline, 1, required_scope=main_scope
            )
            visible_pixel = self.visible_pixel()
            self.x11.move_to_desktop(self.main_window, self.hidden_desktop)
            self.wait_for_condition(
                lambda _: self.x11.desktop(self.main_window) == self.hidden_desktop,
                timeout=10,
            )
            hidden_state = self.wait_native_hidden(self.main_window)
            cycle_entry = self.settle_snapshot(cycle_baseline)
            cycle_hidden = self.settle_snapshot(cycle_entry)
            cycle_scope, cycle_deltas = self.hidden_scope(
                cycle_entry, cycle_hidden, True
            )
            self.assertEqual(cycle_scope, main_scope)
            paused_cycle = self.pause_opposite(visible_pixel)
            restored_pixel = self.restore_and_check(paused_cycle)
            rapid_cycles.append({
                "index": index,
                "hiddenX11": hidden_state,
                "delta": cycle_deltas[main_scope],
                "restorePixel": restored_pixel,
            })
            cycle_baseline = cycle_hidden

        if phase == "workspace":
            other = self.finish_other(other_start)
            self.validate_log()
            self.report.update(
                passed=True,
                phase="workspace",
                mainScope=main_scope,
                otherScope=other_scope,
                otherDeltas=direct_deltas[other_scope],
                workspaceEntryDeltas=entry_deltas[main_scope],
                workspaceDeltas=direct_deltas[main_scope],
                workspaceReadbackDelta=readback_delta,
                workspaceScreenshotPixel=direct_screenshot,
                workspaceCaptureX11=direct_capture_x11,
                workspaceHiddenX11=workspace_hidden_state,
                hiddenResizeDelta=resize_delta,
                hiddenResizeX11=resize_x11,
                workspaceRestorePixel=direct_pixel,
                rapidCycles=rapid_cycles,
                other=other,
            )
            return
