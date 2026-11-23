import array
import os
import socket
import struct
import tempfile

KEYMAP = b'xkb_keymap { xkb_keycodes { include "evdev" }; };'
runtime_dir = os.environ["XDG_RUNTIME_DIR"]
display = os.environ["WAYLAND_DISPLAY"]
path = display if display.startswith("/") else os.path.join(runtime_dir, display)

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)
buffer = bytearray()


def message(object_id, opcode, payload):
    return struct.pack("<II", object_id, ((8 + len(payload)) << 16) | opcode) + payload


def sync(callback_id):
    sock.sendall(message(1, 0, struct.pack("<I", callback_id)))


def receive():
    while True:
        while len(buffer) >= 8:
            object_id, header = struct.unpack_from("<II", buffer)
            length = header >> 16
            if len(buffer) < length:
                break
            opcode = header & 0xFFFF
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
        if descriptors:
            object_id, header = struct.unpack_from("<II", buffer)
            length = header >> 16
            if len(buffer) >= length:
                opcode = header & 0xFFFF
                payload = bytes(buffer[8:length])
                del buffer[:length]
                yield object_id, opcode, payload, descriptors


def wait_for_done(callback_id):
    for object_id, opcode, _, descriptors in receive():
        for fd in descriptors:
            os.close(fd)
        if object_id == callback_id and opcode == 0:
            return


def bind(registry, name, interface, version, object_id):
    encoded = interface.encode() + b"\0"
    encoded += b"\0" * ((4 - len(encoded) % 4) % 4)
    payload = struct.pack("<II", name, len(interface) + 1) + encoded + struct.pack("<II", version, object_id)
    sock.sendall(message(registry, 0, payload))


def present(surface, buffer_id):
    # Waypipe mirrors shared memory on changes reported through a surface: a clean pool or an
    # unattached buffer need not have its contents sent at all.
    sock.sendall(message(surface, 1, struct.pack("<Iii", buffer_id, 0, 0)))
    sock.sendall(message(surface, 2, struct.pack("<iiii", 0, 0, 4, 4)))
    sock.sendall(message(surface, 6, b""))


sock.sendall(message(1, 1, struct.pack("<I", 2)))
sync(3)
wait_for_done(3)

bind(2, 2, "wl_shm", 1, 4)
sync(5)
wait_for_done(5)

bind(2, 1, "wl_compositor", 4, 11)

bind(2, 3, "wl_seat", 7, 12)
sock.sendall(message(12, 1, struct.pack("<I", 13)))
sync(14)
for object_id, opcode, payload, descriptors in receive():
    if object_id == 13 and opcode == 0:
        keymap_format, keymap_size = struct.unpack("<II", payload)
        assert keymap_format == 1
        assert len(descriptors) == 1
        with os.fdopen(descriptors[0], "rb") as keymap:
            assert keymap.read() == KEYMAP[:keymap_size]
        break
    for fd in descriptors:
        os.close(fd)
else:
    raise RuntimeError("keymap stream ended unexpectedly")
wait_for_done(14)

with tempfile.TemporaryFile() as pool:
    pool.write(bytes(range(256)))
    pool.flush()
    create_pool = message(4, 0, struct.pack("<Ii", 6, 256))
    rights = array.array("i", [pool.fileno()])
    sock.sendmsg(
        [create_pool],
        [(socket.SOL_SOCKET, socket.SCM_RIGHTS, rights.tobytes())],
    )
    sock.sendall(message(6, 0, struct.pack("<IiiiiI", 7, 0, 4, 4, 16, 0)))
    sock.sendall(message(11, 0, struct.pack("<I", 15)))
    present(15, 7)
    sync(8)
    wait_for_done(8)

    pool.seek(4)
    pool.write(b"\xaa" * 8)
    pool.flush()
    present(15, 7)
    sync(10)
    wait_for_done(10)

print("remote SHM application completed")
