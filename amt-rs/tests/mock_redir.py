#!/usr/bin/env python3
"""Mock Intel AMT redirection server, used to test the client without hardware.

It implements just enough of the AAAH handshake (0x11/0x13/0x14), the SOL
start (0x20/0x21/0x29) and SOL data (0x28 -> 0x2A echo) to exercise the Rust
client's framing and digest code.

    python3 tests/mock_redir.py --port 16994 --user admin --pass secret --cert cert.pem --key key.pem
"""
import argparse
import hashlib
import socket
import ssl
import struct
import sys

AUTH_URI = "/RedirectionService"
REALM = "Digest:9b8f1a0e"
NONCE = "a1b2c3d4e5f60718"
QOP = "auth"


def md5(s: str) -> str:
    return hashlib.md5(s.encode()).hexdigest()


def frame_0x11(status=0, oem=b"") -> bytes:
    # 13 bytes total: type, status, 10 reserved, oem length at index 12
    body = bytes([0x11, status]) + b"\x00" * 10 + bytes([len(oem)])
    return body + oem


def frame_0x14(status: int, auth_type: int, data: bytes) -> bytes:
    return bytes([0x14, status, 0x00, 0x00, auth_type]) + struct.pack("<I", len(data)) + data


def recv_n(conn, n):
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError("client closed")
        buf += chunk
    return buf


def handle(conn, user, password):
    magic = recv_n(conn, 8)
    proto = "SOL" if magic[4:8] == b"SOL " else magic[4:8].decode(errors="replace")
    print(f"  magic={magic.hex()} proto={proto}", flush=True)

    conn.sendall(frame_0x11())

    # auth query
    req = recv_n(conn, 9)
    assert req[0] == 0x13 and req[4] == 0x00, req.hex()
    conn.sendall(frame_0x14(0, 0, bytes([1, 4])))

    # user + authuri query
    head = recv_n(conn, 9)
    assert head[0] == 0x13 and head[4] == 0x04, head.hex()
    total = struct.unpack("<I", head[5:9])[0]
    body = recv_n(conn, total)
    ulen = body[0]
    cuser = body[1:1 + ulen].decode()
    print(f"  user={cuser}", flush=True)

    conn.sendall(frame_0x14(1, 4, bytes([len(REALM)]) + REALM.encode()
                            + bytes([len(NONCE)]) + NONCE.encode()
                            + bytes([len(QOP)]) + QOP.encode()))

    # digest
    head = recv_n(conn, 9)
    total = struct.unpack("<I", head[5:9])[0]
    body = recv_n(conn, total)
    i = 0

    def field():
        nonlocal i
        n = body[i]
        i += 1
        v = body[i:i + n].decode()
        i += n
        return v

    f_user, f_realm, f_nonce = field(), field(), field()
    f_uri, f_cnonce, f_snc, f_digest, f_qop = field(), field(), field(), field(), field()
    ha1 = md5(f"{f_user}:{f_realm}:{password}")
    ha2 = md5(f"POST:{f_uri}")
    expected = md5(f"{ha1}:{f_nonce}:{f_snc}:{f_cnonce}:{f_qop}:{ha2}")
    ok = (
        f_realm == REALM and f_nonce == NONCE and f_qop == QOP
        and f_uri == AUTH_URI and f_digest == expected
    )
    print(f"  digest={'OK' if ok else 'MISMATCH'} (client={f_digest} expected={expected})", flush=True)
    conn.sendall(frame_0x14(0 if ok else 1, 4, b""))
    if not ok:
        return

    if proto == "SOL":
        # serial settings request
        req = recv_n(conn, 4)
        assert req[0] == 0x20, req.hex()
        recv_n(conn, 20)  # sequence + settings payload (24 bytes with the type)
        conn.sendall(bytes([0x21]) + b"\x00" * 22)
        ack = recv_n(conn, 14)  # 0x27 ack (type + seq + 6 bytes)
        assert ack[0] == 0x27, ack.hex()
        conn.sendall(bytes([0x29]) + b"\x00" * 9)

        # echo whatever the client sends as console output
        while True:
            data = conn.recv(4096)
            if not data:
                return
            if data[0] == 0x28:
                seq = data[4:8]
                ln = struct.unpack("<H", data[8:10])[0]
                payload = data[10:10 + ln]
                print(f"  sol data {payload!r}", flush=True)
                reply = (bytes([0x2A]) + seq + bytes([0x00, 0x00])
                         + struct.pack("<H", len(payload)) + payload)
                conn.sendall(reply)
            elif data[0] == 0x2B:
                print("  keepalive", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=16994)
    ap.add_argument("--user", default="admin")
    ap.add_argument("--pass", dest="password", default="secret")
    ap.add_argument("--cert", required=True)
    ap.add_argument("--key", required=True)
    args = ap.parse_args()

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(args.cert, args.key)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", args.port))
    srv.listen(1)
    print(f"mock AMT redirection on 127.0.0.1:{args.port}", flush=True)
    try:
        raw, addr = srv.accept()
    except KeyboardInterrupt:
        return
    print(f"connection from {addr}", flush=True)
    try:
        conn = ctx.wrap_socket(raw, server_side=True)
        handle(conn, args.user, args.password)
    except EOFError:
        pass
    except Exception as exc:  # noqa: BLE001
        print(f"error: {exc}", file=sys.stderr, flush=True)
    finally:
        print("mock: done", flush=True)


if __name__ == "__main__":
    main()
