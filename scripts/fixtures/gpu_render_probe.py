#!/usr/bin/env python3
"""Render one pixel using surfaceless EGL and report the actual OpenGL renderer."""
import ctypes as C
import os

os.environ["EGL_PLATFORM"] = "surfaceless"
egl = C.CDLL("libEGL.so.1")
gl = C.CDLL("libGL.so.1")


def api(lib, name, result, *args):
    fn = getattr(lib, name)
    fn.restype = result
    fn.argtypes = args
    return fn


ptr, integer = C.c_void_p, C.c_int
display = api(egl, "eglGetDisplay", ptr, ptr)(None)
assert api(egl, "eglInitialize", C.c_uint, ptr, ptr, ptr)(display, None, None)
assert api(egl, "eglBindAPI", C.c_uint, C.c_uint)(0x30A2)  # OpenGL
attrs = (integer * 9)(0x3033, 1, 0x3040, 8, 0x3024, 8, 0x3025, 8, 0x3038)
config, count = ptr(), integer()
assert api(egl, "eglChooseConfig", C.c_uint, ptr, ptr, ptr, integer, ptr)(
    display, attrs, C.byref(config), 1, C.byref(count)
) and count.value
pattrs = (integer * 5)(0x3057, 1, 0x3056, 1, 0x3038)
surface = api(egl, "eglCreatePbufferSurface", ptr, ptr, ptr, ptr)(display, config, pattrs)
context = api(egl, "eglCreateContext", ptr, ptr, ptr, ptr, ptr)(display, config, None, None)
assert surface and context
assert api(egl, "eglMakeCurrent", C.c_uint, ptr, ptr, ptr, ptr)(display, surface, surface, context)
renderer = api(gl, "glGetString", C.c_char_p, C.c_uint)(0x1F01).decode()
print("RENDERER:", renderer, flush=True)
api(gl, "glClearColor", None, C.c_float, C.c_float, C.c_float, C.c_float)(1, 0, 0, 1)
api(gl, "glClear", None, C.c_uint)(0x4000)
pixel = (C.c_ubyte * 4)()
api(gl, "glReadPixels", None, integer, integer, integer, integer, C.c_uint, C.c_uint, ptr)(
    0, 0, 1, 1, 0x1908, 0x1401, pixel
)
assert list(pixel)[:3] == [255, 0, 0], list(pixel)
print("PIXEL: red", flush=True)
assert not any(s in renderer.lower() for s in ("llvmpipe", "softpipe", "swrast", "software")), renderer
api(egl, "eglMakeCurrent", C.c_uint, ptr, ptr, ptr, ptr)(display, None, None, None)
api(egl, "eglDestroyContext", C.c_uint, ptr, ptr)(display, context)
api(egl, "eglDestroySurface", C.c_uint, ptr, ptr)(display, surface)
api(egl, "eglTerminate", C.c_uint, ptr)(display)
