# Intel AMT KVM video: what the reference actually does

Extracted from MeshCommander's `default.htm` (Apache-2.0), functions around
`StartRedirectionSession` and the desktop module. This is the map we follow in
`crates/amt-kvm`; the encodings still to decode are marked TODO.

## Transport

After the AAAH handshake on **16995** (TLS; 16994 is the plain-text port) with the `KVMR` magic, the client sends
`0x40` (KVM enable), receives `0x41`, and from that point the stream is **raw
RFB (VNC) traffic inside the redirection session** — no Intel framing any more.
MeshCommander forwards the remaining bytes straight to the RFB parser.

So the video path is a standard RFB client plus Intel-specific encodings. That
is the single most useful finding: the protocol is documented, and only the
encodings are Intel's.

## RFB handshake (as implemented by the reference)

1. Server sends the RFB version string.
2. Client replies with its version.
3. Security: server offers types; the client sends `1` (None) — the real
   authentication already happened in the redirection handshake.
4. Client reads 4 bytes: if the u32 is not 0, abort (security failed).
5. Client sends `1` = shared desktop flag.
6. Server init: 24 bytes + name. Width `u16` at 0, height `u16` at 2, pixel
   format at 4..20, name length `u32` at 20, name follows.
   The reference ignores the pixel format and always works in RGB332/RGB565.

## Encodings

`SetEncodings` must always include **RAW (0)**: Intel AMT does not accept a
list without it. The reference also advertises RLE (16) and Desktop Size
(`0xFFFFFF21`, i.e. -223).

Intel encoding numbers (from the UI menu):

| Value | Encoding |
|---|---|
| 1 | RLE8, 256 colours |
| 3 | RLE16, 64k colours |
| 5 | RLE8G, 256 greys |
| 9 | ZRLE8, compressed colour |
| 11 | ZRLE16, compressed colour |
| 13 | ZRLE8G, compressed grey |
| 21 | RLE4G, 16 greys |
| 29 | ZRLE4G, compressed grey |

Framework messages are standard RFB: `0` FramebufferUpdate, `1` SetColourMap,
`2` Bell, `3` ServerCutText. FramebufferUpdate is a sequence of rectangles;
each rectangle carries `x, y, w, h, encoding`, and the payload depends on the
encoding.

## Still to do

- RLE8 / RLE16 rectangle payload layout (colour map handling, run encoding)
- ZRLE (zlib) variants
- Desktop Size pseudo-encoding handling (resolution changes mid-session)
- Input: `4` KeyEvent and `5` PointerEvent frames, PS/2 set 1 scancodes and
  absolute coordinates (0..65535)
- IDER: separate protocol (`IDER` magic, 0x10/0x11 handshake), untouched

## Why this matters for the port

The RFB core can be written once and tested with synthetic frames; the Intel
encodings are small functions over that core. Nothing here needs the Intel
redirection framing after the KVM enable, which is why the GUI can be a plain
Rust window (winit + softbuffer) on macOS and Windows without platform tricks.
