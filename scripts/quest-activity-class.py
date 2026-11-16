#!/usr/bin/env python3
"""Writes com/oculus/NativeActivity.class: an empty subclass of android.app.NativeActivity.

Horizon OS only treats an app as an immersive launch when its activity class is named
com.oculus.NativeActivity; an activity-alias with that name is placed as a 2D panel instead.
The class has no behaviour beyond its name, so it is generated here rather than written in Java.
Usage: quest-activity-class.py <out-dir>
"""
import pathlib
import struct
import sys

THIS = "com/oculus/NativeActivity"
SUPER = "android/app/NativeActivity"


def utf8(s):
    data = s.encode()
    return struct.pack(">BH", 1, len(data)) + data


pool = [
    struct.pack(">BH", 7, 2),  # 1 Class -> 2
    utf8(THIS),  # 2
    struct.pack(">BH", 7, 4),  # 3 Class -> 4
    utf8(SUPER),  # 4
    utf8("<init>"),  # 5
    utf8("()V"),  # 6
    struct.pack(">BHH", 12, 5, 6),  # 7 NameAndType <init> ()V
    struct.pack(">BHH", 10, 3, 7),  # 8 Methodref super.<init>()V
    utf8("Code"),  # 9
]

code = bytes([0x2A, 0xB7, 0x00, 0x08, 0xB1])  # aload_0; invokespecial #8; return
code_attr = struct.pack(">HHI", 1, 1, len(code)) + code + struct.pack(">HH", 0, 0)
init = struct.pack(">HHHH", 0x0001, 5, 6, 1) + struct.pack(">HI", 9, len(code_attr)) + code_attr

classfile = (
    struct.pack(">IHH", 0xCAFEBABE, 0, 52)
    + struct.pack(">H", len(pool) + 1)
    + b"".join(pool)
    + struct.pack(">HHHHH", 0x0021, 1, 3, 0, 0)  # public super; this; super; no interfaces/fields
    + struct.pack(">H", 1)
    + init
    + struct.pack(">H", 0)
)

out = pathlib.Path(sys.argv[1]) / "com" / "oculus" / "NativeActivity.class"
out.parent.mkdir(parents=True, exist_ok=True)
out.write_bytes(classfile)
print(out)
