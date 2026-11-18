import array
import os
import socket
import struct

REMOTE_TEXT = b"Waypipe clipboard"
HOST_TEXT = b"from host"
TEXT_MIME = "text/plain;charset=utf-8"

runtime_dir = os.environ["XDG_RUNTIME_DIR"]
display = os.environ["WAYLAND_DISPLAY"]
path = display if display.startswith("/") else os.path.join(runtime_dir, display)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)
buffer = bytearray()


def message(object_id, opcode, payload):
    return struct.pack("<II", object_id, ((8 + len(payload)) << 16) | opcode) + payload


def pack_string(value):
    encoded = value.encode() + b"\0"
    encoded += b"\0" * ((4 - len(encoded) % 4) % 4)
    return struct.pack("<I", len(value.encode()) + 1) + encoded


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
        data, ancillary, _, _ = sock.recvmsg(65536, socket.CMSG_SPACE(16 * array.array("i").itemsize))
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


def bind(name, interface, version, object_id):
    encoded = interface.encode() + b"\0"
    encoded += b"\0" * ((4 - len(encoded) % 4) % 4)
    payload = struct.pack("<II", name, len(interface) + 1) + encoded + struct.pack("<II", version, object_id)
    sock.sendall(message(2, 0, payload))


def wait_done(callback_id):
    for object_id, opcode, _, descriptors in events():
        for fd in descriptors:
            os.close(fd)
        if object_id == callback_id and opcode == 0:
            return


def wait_selection():
    current_offer = None
    for object_id, opcode, payload, descriptors in events():
        for fd in descriptors:
            os.close(fd)
        if object_id == 12 and opcode == 0:
            current_offer = struct.unpack("<I", payload)[0]
            assert current_offer >= 0xFF000000
        elif object_id == 12 and opcode == 5:
            selected = struct.unpack("<I", payload)[0]
            if selected != 0:
                assert selected == current_offer
                return current_offer


def read_clipboard(offer_id):
    read_fd, write_fd = os.pipe()
    rights = array.array("i", [write_fd])
    sock.sendmsg(
        [message(offer_id, 1, pack_string(TEXT_MIME))],
        [(socket.SOL_SOCKET, socket.SCM_RIGHTS, rights.tobytes())],
    )
    os.close(write_fd)
    for object_id, opcode, payload, descriptors in events():
        if object_id == 13 and opcode == 1:
            assert descriptors, "wl_data_source.send did not deliver a pipe fd"
            os.write(descriptors[0], REMOTE_TEXT)
            os.close(descriptors[0])
            for fd in descriptors[1:]:
                os.close(fd)
            break
        for fd in descriptors:
            os.close(fd)
    data = os.read(read_fd, 4096)
    os.close(read_fd)
    return data


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
assert {"wl_compositor", "xdg_wm_base", "wl_seat", "wl_data_device_manager"} <= globals_by_name.keys()

bind(globals_by_name["wl_compositor"], "wl_compositor", 5, 4)
bind(globals_by_name["xdg_wm_base"], "xdg_wm_base", 5, 5)
bind(globals_by_name["wl_seat"], "wl_seat", 7, 6)
bind(globals_by_name["wl_data_device_manager"], "wl_data_device_manager", 3, 7)
sync(8)
wait_done(8)

sock.sendall(message(4, 0, struct.pack("<I", 9)))
sock.sendall(message(5, 2, struct.pack("<II", 10, 9)))
sock.sendall(message(10, 1, struct.pack("<I", 11)))
for object_id, opcode, payload, _ in events():
    if object_id == 10 and opcode == 0:
        serial = struct.unpack("<I", payload)[0]
        sock.sendall(message(10, 4, struct.pack("<I", serial)))
        break

sock.sendall(message(7, 1, struct.pack("<II", 12, 6)))
sock.sendall(message(7, 0, struct.pack("<I", 13)))
sock.sendall(message(13, 0, pack_string(TEXT_MIME)))
sock.sendall(message(12, 1, struct.pack("<II", 13, 0)))
offer = wait_selection()
assert read_clipboard(offer) == REMOTE_TEXT

offer = wait_selection()
read_fd, write_fd = os.pipe()
rights = array.array("i", [write_fd])
sock.sendmsg(
    [message(offer, 1, pack_string(TEXT_MIME))],
    [(socket.SOL_SOCKET, socket.SCM_RIGHTS, rights.tobytes())],
)
os.close(write_fd)
assert os.read(read_fd, len(HOST_TEXT) + 1) == HOST_TEXT
os.close(read_fd)

sock.sendall(message(13, 1, b""))
sock.sendall(message(12, 2, b""))
sock.sendall(message(11, 0, b""))
sock.sendall(message(10, 0, b""))
sock.sendall(message(9, 0, b""))
sync(31)
wait_done(31)
print("remote clipboard application completed")
