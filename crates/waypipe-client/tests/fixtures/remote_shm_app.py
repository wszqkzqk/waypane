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


def message(object_id, opcode, payload):
    return struct.pack("<II", object_id, ((8 + len(payload)) << 16) | opcode) + payload


def sync(callback_id):
    sock.sendall(message(1, 0, struct.pack("<I", callback_id)))


def wait_for_done(callback_id):
    while True:
        while len(buffer) >= 8:
            object_id, header = struct.unpack_from("<II", buffer)
            length = header >> 16
            if len(buffer) < length:
                break
            opcode = header & 0xFFFF
            del buffer[:length]
            if object_id == callback_id and opcode == 0:
                return
        chunk = sock.recv(65536)
        if not chunk:
            raise RuntimeError(f"Wayland socket closed before callback {callback_id}")
        buffer.extend(chunk)


sock.sendall(message(1, 1, struct.pack("<I", 2)))
sync(3)
wait_for_done(3)

interface = b"wl_shm\0"
padding = b"\0" * ((4 - len(interface) % 4) % 4)
bind = struct.pack("<II", 2, len(interface)) + interface + padding + struct.pack("<II", 1, 4)
sock.sendall(message(2, 0, bind))
sync(5)
wait_for_done(5)

with tempfile.TemporaryFile() as pool:
    pool.write(bytes(range(256)))
    pool.flush()
    create_pool = message(4, 0, struct.pack("<Ii", 6, 256))
    sock.sendmsg([create_pool], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("@i", pool.fileno()))])
    sock.sendall(message(6, 0, struct.pack("<IiiiiI", 7, 0, 4, 4, 16, 0)))
    sync(8)
    wait_for_done(8)

    pool.seek(4)
    pool.write(b"\xaa" * 8)
    pool.flush()
    sock.sendall(message(6, 0, struct.pack("<IiiiiI", 9, 0, 4, 4, 16, 0)))
    sync(10)
    wait_for_done(10)

print("remote SHM application completed")
