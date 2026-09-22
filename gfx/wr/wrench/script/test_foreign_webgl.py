# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import ctypes as C
import argparse
import json
import os
import subprocess

P, I, U = C.c_void_p, C.c_int, C.c_uint


def bind(lib, name, result, args):
    fn = getattr(lib, name)
    fn.restype, fn.argtypes = result, args
    return fn


def proc(egl, name, result, args):
    address = egl.eglGetProcAddress(name.encode())
    if not address:
        raise RuntimeError("Missing EGL/GL function " + name)
    return C.CFUNCTYPE(result, *args)(address)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument(
        "--test",
        default="device::hal::vulkan::linux::foreign_rgb::gpu_tests::gl_dmabuf_direct_sampling_and_release",
    )
    parser.add_argument("--generations", type=int, default=2)
    parser.add_argument("--destructive", action="store_true")
    args = parser.parse_args()
    if args.destructive and args.generations != 1:
        parser.error("Destructive tests require exactly one generation")
    binary = args.binary
    egl, gbm, gl = (
        C.CDLL(name) for name in ["libEGL.so.1", "libgbm.so.1", "libGLESv2.so.2"]
    )
    bind(egl, "eglGetProcAddress", P, [C.c_char_p])
    display_for = proc(egl, "eglGetPlatformDisplayEXT", P, [U, P, C.POINTER(I)])
    init = bind(egl, "eglInitialize", U, [P, C.POINTER(I), C.POINTER(I)])
    api = bind(egl, "eglBindAPI", U, [U])
    context = bind(egl, "eglCreateContext", P, [P, P, P, C.POINTER(I)])
    current = bind(egl, "eglMakeCurrent", U, [P, P, P, P])
    create_image = proc(egl, "eglCreateImageKHR", P, [P, P, U, P, C.POINTER(I)])
    destroy_image = proc(egl, "eglDestroyImageKHR", U, [P, P])
    create_sync = proc(egl, "eglCreateSyncKHR", P, [P, U, C.POINTER(I)])
    dup_sync = proc(egl, "eglDupNativeFenceFDANDROID", I, [P, P])
    destroy_sync = proc(egl, "eglDestroySyncKHR", U, [P, P])
    image_target = proc(egl, "glEGLImageTargetTexture2DOES", None, [U, P])
    gbm_device = bind(gbm, "gbm_create_device", P, [I])
    create_bo = bind(
        gbm,
        "gbm_bo_create_with_modifiers2",
        P,
        [P, U, U, U, C.POINTER(C.c_uint64), U, U],
    )
    bo_fd = bind(gbm, "gbm_bo_get_fd", I, [P])
    bo_pitch = bind(gbm, "gbm_bo_get_stride", U, [P])
    bo_modifier = bind(gbm, "gbm_bo_get_modifier", C.c_uint64, [P])
    bo_planes = bind(gbm, "gbm_bo_get_plane_count", I, [P])
    bo_destroy = bind(gbm, "gbm_bo_destroy", None, [P])
    for name, result, signature in [
        ("glGenTextures", None, [I, C.POINTER(U)]),
        ("glBindTexture", None, [U, U]),
        ("glGenFramebuffers", None, [I, C.POINTER(U)]),
        ("glBindFramebuffer", None, [U, U]),
        ("glFramebufferTexture2D", None, [U, U, U, U, I]),
        ("glCheckFramebufferStatus", U, [U]),
        ("glDisable", None, [U]),
        ("glEnable", None, [U]),
        ("glScissor", None, [I, I, I, I]),
        ("glClearColor", None, [C.c_float] * 4),
        ("glClear", None, [U]),
        ("glFlush", None, []),
        ("glReadPixels", None, [I, I, I, I, U, U, P]),
        ("glGetError", U, []),
        ("glGetString", C.c_char_p, [U]),
        ("glDeleteTextures", None, [I, C.POINTER(U)]),
        ("glDeleteFramebuffers", None, [I, C.POINTER(U)]),
    ]:
        bind(gl, name, result, signature)
    render_fd = os.open(
        os.environ.get("WR_GBM_NODE", "/dev/dri/renderD128"), os.O_RDWR | os.O_CLOEXEC
    )
    device = gbm_device(render_fd)
    assert device
    display = display_for(0x31D7, device, None)
    assert display and init(display, None, None)
    assert api(0x30A0)
    ctx = context(display, None, None, (I * 3)(0x3098, 2, 0x3038))
    assert ctx and current(display, None, None, ctx)
    print("GL producer:", gl.glGetString(0x1F01).decode(), flush=True)
    width, height = 17, 9
    for fourcc in [
        int.from_bytes(b"AR24", "little"),
        int.from_bytes(b"AB24", "little"),
    ]:
        bo = create_bo(device, width, height, fourcc, (C.c_uint64 * 1)(0), 1, 4)
        assert bo and bo_modifier(bo) == 0 and bo_planes(bo) == 1
        fd, pitch = bo_fd(bo), bo_pitch(bo)
        assert fd >= 0
        attrs = [
            0x3057,
            width,
            0x3056,
            height,
            0x3271,
            fourcc,
            0x3272,
            fd,
            0x3273,
            0,
            0x3274,
            pitch,
            0x3443,
            0,
            0x3444,
            0,
            0x3038,
        ]
        image = create_image(display, None, 0x3270, None, (I * len(attrs))(*attrs))
        assert image
        tex, fbo = U(), U()
        gl.glGenTextures(1, C.byref(tex))
        gl.glBindTexture(0x0DE1, tex)
        image_target(0x0DE1, image)
        gl.glGenFramebuffers(1, C.byref(fbo))
        gl.glBindFramebuffer(0x8D40, fbo)
        gl.glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, tex, 0)
        assert gl.glCheckFramebufferStatus(0x8D40) == 0x8CD5
        for generation in range(1, args.generations + 1):
            gl.glDisable(0x0BD0)
            gl.glDisable(0x0BE2)
            gl.glDisable(0x0C11)
            gl.glClearColor(0, 0, 1 if generation == 1 else 0, 1)
            gl.glClear(0x4000)
            gl.glEnable(0x0C11)
            gl.glScissor(0, 0, 5, 4)
            gl.glClearColor(1, 0, 0, 1)
            gl.glClear(0x4000)
            gl.glScissor(7, 2, 3, 3)
            gl.glClearColor(0, 1, 0, 1)
            gl.glClear(0x4000)
            sync = create_sync(display, 0x3144, (I * 1)(0x3038))
            assert sync
            gl.glFlush()
            fence = dup_sync(display, sync)
            assert fence >= 0
            destroy_sync(display, sync)
            env = dict(
                os.environ,
                WR_FOREIGN_RGB_FD=str(fd),
                WR_FOREIGN_RGB_FENCE=str(fence),
                WR_FOREIGN_RGB_FOURCC=str(fourcc),
                WR_FOREIGN_RGB_PITCH=str(pitch),
                WR_FOREIGN_RGB_GENERATION=str(generation),
            )
            print(
                json.dumps(
                    {"fourcc": fourcc, "pitch": pitch, "generation": generation}
                ),
                flush=True,
            )
            child = subprocess.run(
                [
                    binary,
                    "--ignored",
                    "--exact",
                    args.test,
                    "--nocapture",
                    "--test-threads=1",
                ],
                env=env,
                pass_fds=(fd, fence),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            print(child.stdout, end="", flush=True)
            assert child.returncode == 0 and "1 passed; 0 failed" in child.stdout
            os.close(fence)
            if args.destructive:
                continue
            actual = (C.c_ubyte * (width * height * 4))()
            gl.glReadPixels(0, 0, width, height, 0x1908, 0x1401, actual)
            expected = []
            for y in range(height):
                for x in range(width):
                    expected += (
                        [0, 255, 0, 255]
                        if 7 <= x < 10 and 2 <= y < 5
                        else [255, 0, 0, 255]
                        if x < 5 and y < 4
                        else [0, 0, 255 if generation == 1 else 0, 255]
                    )
            assert bytes(actual) == bytes(expected)
            assert gl.glGetError() == 0
        gl.glDeleteFramebuffers(1, C.byref(fbo))
        gl.glDeleteTextures(1, C.byref(tex))
        destroy_image(display, image)
        os.close(fd)
        bo_destroy(bo)
    print(f"Foreign WebGL fixture passed: {args.test}", flush=True)


if __name__ == "__main__":
    main()
