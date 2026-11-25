# Waypane: window-level Wayland forwarding over SSH.
# Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.
#
# SPDX-License-Identifier: GPL-3.0-or-later

import os
import socket
import struct

PREEDIT = "拼音"
COMMIT = "你好，Waypane 🚀"

runtime_dir = os.environ["XDG_RUNTIME_DIR"]
display = os.environ["WAYLAND_DISPLAY"]
path = display if display.startswith("/") else os.path.join(runtime_dir, display)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)
buffer = bytearray()


def message(object_id, opcode, payload):
    return struct.pack("<II", object_id, ((8 + len(payload)) << 16) | opcode) + payload


def pack_string(value):
    encoded = value.encode("utf-8") + b"\0"
    encoded += b"\0" * ((4 - len(encoded) % 4) % 4)
    return struct.pack("<I", len(value.encode("utf-8")) + 1) + encoded


def read_string(payload, offset=0):
    length = struct.unpack_from("<I", payload, offset)[0]
    if length == 0:
        return "", offset + 4
    start = offset + 4
    text = payload[start:start + length - 1].decode("utf-8")
    padded = (length + 3) & ~3
    return text, start + padded


def sync(callback_id):
    sock.sendall(message(1, 0, struct.pack("<I", callback_id)))


def events():
    while True:
        while len(buffer) >= 8:
            object_id, header = struct.unpack_from("<II", buffer)
            length = header >> 16
            if len(buffer) < length:
                break
            opcode = header & 0xFF
            payload = bytes(buffer[8:length])
            del buffer[:length]
            yield object_id, opcode, payload
        chunk = sock.recv(65536)
        if not chunk:
            raise RuntimeError("Wayland socket closed")
        buffer.extend(chunk)


def bind(name, interface, version, object_id):
    encoded = interface.encode() + b"\0"
    encoded += b"\0" * ((4 - len(encoded) % 4) % 4)
    payload = struct.pack("<II", name, len(interface) + 1) + encoded + struct.pack("<II", version, object_id)
    sock.sendall(message(2, 0, payload))


def wait_done(callback_id):
    for object_id, opcode, payload in events():
        if object_id == callback_id and opcode == 0:
            return


sock.sendall(message(1, 1, struct.pack("<I", 2)))
sync(3)
globals_by_name = {}
for object_id, opcode, payload in events():
    if object_id == 2 and opcode == 0:
        name, size = struct.unpack_from("<II", payload)
        interface = payload[8:8 + size - 1].decode()
        globals_by_name[interface] = name
    if object_id == 3 and opcode == 0:
        break
assert {"wl_compositor", "xdg_wm_base", "wl_seat", "zwp_text_input_manager_v3"} <= globals_by_name.keys()

bind(globals_by_name["wl_compositor"], "wl_compositor", 5, 4)
bind(globals_by_name["xdg_wm_base"], "xdg_wm_base", 5, 5)
bind(globals_by_name["wl_seat"], "wl_seat", 7, 6)
bind(globals_by_name["zwp_text_input_manager_v3"], "zwp_text_input_manager_v3", 1, 30)
sync(8)
wait_done(8)

sock.sendall(message(4, 0, struct.pack("<I", 9)))
sock.sendall(message(5, 2, struct.pack("<II", 10, 9)))
sock.sendall(message(10, 1, struct.pack("<I", 11)))
for object_id, opcode, payload in events():
    if object_id == 10 and opcode == 0:
        serial = struct.unpack("<I", payload)[0]
        sock.sendall(message(10, 4, struct.pack("<I", serial)))
        break

sock.sendall(message(30, 1, struct.pack("<II", 31, 6)))
for object_id, opcode, payload in events():
    if object_id == 31 and opcode == 0:
        assert struct.unpack("<I", payload)[0] == 9
        break

sock.sendall(message(31, 1, b""))
sock.sendall(message(31, 7, b""))
for object_id, opcode, payload in events():
    if object_id == 31 and opcode == 5:
        assert struct.unpack("<I", payload)[0] == 1
        break

preedit_seen = False
commit_seen = False
for object_id, opcode, payload in events():
    if object_id == 31 and opcode == 2:
        text, offset = read_string(payload)
        begin, end = struct.unpack_from("<ii", payload, offset)
        if text == PREEDIT:
            assert (begin, end) == (0, len(PREEDIT.encode("utf-8")))
            preedit_seen = True
        else:
            assert text == "" and (begin, end) == (-1, -1)
    elif object_id == 31 and opcode == 3:
        text, _ = read_string(payload)
        assert text == COMMIT
        commit_seen = True
    elif object_id == 31 and opcode == 5:
        assert struct.unpack("<I", payload)[0] == 1
        if preedit_seen and commit_seen:
            break

sock.sendall(message(31, 0, b""))
sock.sendall(message(11, 0, b""))
sock.sendall(message(10, 0, b""))
sock.sendall(message(9, 0, b""))
sync(32)
wait_done(32)
print("remote text input application completed")
