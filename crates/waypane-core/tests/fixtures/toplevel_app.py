import array
import os
import socket
import struct
import tempfile

runtime_dir = os.environ["XDG_RUNTIME_DIR"]
display = os.environ["WAYLAND_DISPLAY"]
path = display if display.startswith("/") else os.path.join(runtime_dir, display)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)
buffer = bytearray()
saw_keyboard_enter = False
saw_pointer_enter = False
saw_key = False
keyboard_events = []


def message(object_id, opcode, payload):
    return struct.pack("<II", object_id, ((8 + len(payload)) << 16) | opcode) + payload


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
            yield object_id, opcode, payload, []
        data, ancillary, _, _ = sock.recvmsg(65536, socket.CMSG_SPACE(64 * array.array("i").itemsize))
        if not data:
            raise RuntimeError("Wayland socket closed")
        buffer.extend(data)
        descriptors = []
        for level, kind, cmsg in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                ints = array.array("i")
                ints.frombytes(cmsg[: len(cmsg) - (len(cmsg) % ints.itemsize)])
                descriptors.extend(ints)
        if descriptors and len(buffer) >= 8:
            object_id, header = struct.unpack_from("<II", buffer)
            length = header >> 16
            if len(buffer) >= length:
                opcode = header & 0xFF
                payload = bytes(buffer[8:length])
                del buffer[:length]
                yield object_id, opcode, payload, descriptors


def track_input(object_id, opcode):
    global saw_keyboard_enter, saw_pointer_enter, saw_key
    if object_id == 13:
        keyboard_events.append((opcode, payload.hex()))
    if object_id == 13 and opcode == 1:
        saw_keyboard_enter = True
    if object_id == 24 and opcode == 0:
        saw_pointer_enter = True
        serial = struct.unpack_from("<I", payload)[0]
        sock.sendall(message(24, 0, struct.pack("<IIii", serial, 0, 0, 0)))
    if object_id == 13 and opcode == 3:
        assert len(payload) == 16, (
            f"wl_keyboard.key payload has {len(payload)} bytes: {payload.hex()}; "
            f"keyboard events: {keyboard_events}"
        )
        _, _, key, state = struct.unpack("<IIII", payload)
        assert (key, state) == (42, 1)
        saw_key = True


def wait_done(callback_id):
    for object_id, opcode, payload, descriptors in events():
        for fd in descriptors:
            os.close(fd)
        track_input(object_id, opcode)
        if object_id == callback_id and opcode == 0:
            return


def bind(registry, name, interface, version, object_id):
    encoded = interface.encode() + b"\0"
    encoded += b"\0" * ((4 - len(encoded) % 4) % 4)
    payload = struct.pack("<II", name, len(interface) + 1) + encoded + struct.pack("<II", version, object_id)
    sock.sendall(message(registry, 0, payload))


sock.sendall(message(1, 1, struct.pack("<I", 2)))
sync(3)
globals_by_name = {}
for object_id, opcode, payload, _ in events():
    if object_id == 2 and opcode == 0:
        name, size = struct.unpack_from("<II", payload)
        interface = payload[8:8 + size - 1].decode()
        globals_by_name[interface] = name
    if object_id == 3 and opcode == 0:
        break
assert {"wl_compositor", "wl_shm", "xdg_wm_base", "wl_seat", "wl_output"} <= globals_by_name.keys()

bind(2, globals_by_name["wl_compositor"], "wl_compositor", 5, 4)
bind(2, globals_by_name["wl_shm"], "wl_shm", 1, 5)
bind(2, globals_by_name["xdg_wm_base"], "xdg_wm_base", 5, 6)
bind(2, globals_by_name["wl_seat"], "wl_seat", 7, 7)
bind(2, globals_by_name["wl_output"], "wl_output", 4, 25)
sock.sendall(message(7, 1, struct.pack("<I", 13)))
sock.sendall(message(7, 0, struct.pack("<I", 24)))
sync(14)
for object_id, opcode, payload, descriptors in events():
    if object_id == 13 and opcode == 0:
        keymap_format, keymap_size = struct.unpack("<II", payload)
        assert keymap_format == 1 and len(descriptors) == 1
        with os.fdopen(descriptors[0], "rb") as keymap:
            content = keymap.read()
            assert len(content) == keymap_size and b'pc+us+inet(evdev)' in content
        break
    for fd in descriptors:
        os.close(fd)
wait_done(14)

sock.sendall(message(4, 0, struct.pack("<I", 15)))
sock.sendall(message(6, 2, struct.pack("<II", 16, 15)))
sock.sendall(message(16, 1, struct.pack("<I", 17)))
for object_id, opcode, payload, _ in events():
    if object_id == 16 and opcode == 0:
        serial = struct.unpack("<I", payload)[0]
        sock.sendall(message(16, 4, struct.pack("<I", serial)))
        break
title = b"Waypane E2E\0"
app_id = b"waypane-test\0" + b"\0\0\0"
sock.sendall(message(17, 2, struct.pack("<I", len(b"Waypane E2E\0")) + title))
sock.sendall(message(17, 3, struct.pack("<I", len(b"waypane-test\0")) + app_id))

with tempfile.TemporaryFile() as pool:
    pool.write(bytes(range(256)) * 16)
    pool.flush()
    create_pool = message(5, 0, struct.pack("<Ii", 18, 4096))
    rights = array.array("i", [pool.fileno()])
    sock.sendmsg([create_pool], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, rights.tobytes())])
    sock.sendall(message(18, 0, struct.pack("<IiiiiI", 20, 0, 32, 32, 128, 1)))
    sock.sendall(message(15, 1, struct.pack("<Iii", 20, 0, 0)))
    sock.sendall(message(15, 9, struct.pack("<iiii", 0, 0, 32, 32)))
    sock.sendall(message(15, 3, struct.pack("<I", 21)))
    sock.sendall(message(15, 6, b""))

    released = done = False
    for object_id, opcode, _, descriptors in events():
        for fd in descriptors:
            os.close(fd)
        released |= object_id == 20 and opcode == 0
        done |= object_id == 21 and opcode == 0
        if released and done:
            break

    pool.seek(4)
    pool.write(b"\xaa" * 8)
    pool.flush()
    sock.sendall(message(18, 0, struct.pack("<IiiiiI", 22, 0, 32, 32, 128, 1)))
    sock.sendall(message(15, 1, struct.pack("<Iii", 22, 0, 0)))
    sock.sendall(message(15, 3, struct.pack("<I", 23)))
    sock.sendall(message(15, 6, b""))
    released = done = False
    for object_id, opcode, _, descriptors in events():
        for fd in descriptors:
            os.close(fd)
        track_input(object_id, opcode)
        released |= object_id == 22 and opcode == 0
        done |= object_id == 23 and opcode == 0
        if released and done:
            break

    sock.sendall(message(20, 0, b""))
    sock.sendall(message(22, 0, b""))
    sock.sendall(message(17, 0, b""))
    sock.sendall(message(16, 0, b""))
    sock.sendall(message(15, 0, b""))
    sock.sendall(message(18, 1, b""))
    sync(26)
    wait_done(26)

if os.environ.get("WAYPANE_SKIP_INPUT") != "1":
    assert saw_keyboard_enter and saw_pointer_enter and saw_key
print("remote xdg toplevel application completed")
