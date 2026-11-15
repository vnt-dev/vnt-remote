"""Capture an isolated 1600x1000 Xvfb display and verify live fixture pixels."""
import ctypes
import sys
from pathlib import Path

from PIL import Image, ImageChops

x11 = ctypes.CDLL("libX11.so.6")
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x11.XDefaultRootWindow.restype = ctypes.c_ulong


class XImage(ctypes.Structure):
    _fields_ = [
        ("width", ctypes.c_int), ("height", ctypes.c_int),
        ("xoffset", ctypes.c_int), ("format", ctypes.c_int),
        ("data", ctypes.c_void_p), ("byte_order", ctypes.c_int),
        ("bitmap_unit", ctypes.c_int), ("bitmap_bit_order", ctypes.c_int),
        ("bitmap_pad", ctypes.c_int), ("depth", ctypes.c_int),
        ("bytes_per_line", ctypes.c_int), ("bits_per_pixel", ctypes.c_int),
    ]


x11.XGetImage.argtypes = [
    ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
    ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int,
]
x11.XGetImage.restype = ctypes.POINTER(XImage)
x11.XDestroyImage.argtypes = [ctypes.POINTER(XImage)]
x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
display = x11.XOpenDisplay(None)
assert display, "Xvfb display must be available"
native = x11.XGetImage(display, x11.XDefaultRootWindow(display), 0, 0,
                      1600, 1000, ctypes.c_ulong(-1), 2)
assert native and native.contents.bits_per_pixel == 32
raw = ctypes.string_at(native.contents.data, native.contents.bytes_per_line * 1000)
image = Image.frombytes("RGB", (1600, 1000), raw, "raw", "BGRX",
                        native.contents.bytes_per_line, 1)
x11.XDestroyImage(native)
x11.XCloseDisplay(display)
output = Path(sys.argv[1])
output.parent.mkdir(parents=True, exist_ok=True)
image.save(output)
samples = list(image.crop((20, 80, 1200, 720)).resize((118, 64)).getdata())
colorful = sum(max(pixel) - min(pixel) > 80 and max(pixel) > 100 for pixel in samples)
assert colorful > len(samples) // 4, "Remote canvas must display the fixture, not a blank/error page"
if len(sys.argv) > 2:
    before = Image.open(sys.argv[2]).crop((20, 80, 1200, 720))
    diff = ImageChops.difference(before, image.crop((20, 80, 1200, 720))).resize((118, 64))
    assert sum(max(pixel) > 8 for pixel in diff.getdata()) > len(samples) // 100, "Remote video must keep updating"
