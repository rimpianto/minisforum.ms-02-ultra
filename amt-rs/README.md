# amt-rs — a native Intel AMT client

Rust implementation of the AMT operations we actually need on the
Minisforum MS-02 Ultra: power control now, KVM / SOL / IDER as the
protocol work lands. One static binary, no Node.js runtime, no browser.

MeshCommander remains the reference implementation and the only usable
console today, but its KVM is not workable for real operations
(no copy-paste, poor rendering) and it drags in a Node.js process. The
wire protocols are documented and small enough to own.

## State

Verified against the AMT of this host (dedicated management LAN,
`192.168.99.153`), October 2026. Mock = `tests/mock_redir.py`,
hardware = the real device.

| Part | State |
|---|---|
| WS-Man power control (`on` / `off` / `reset`) | **works on hardware** — HTTP 200, `ReturnValue 0` |
| `ReturnValue` validation (client no longer trusts "HTTP 2xx = success") | done |
| Power-state query (`Get` on `CIM_AssociatedPowerManagementService`) | **works on hardware** — prints `PowerState=N (name)` |
| Redirection transport + AAAH handshake (SOL, KVM) | **works on hardware** — TLS on `16995` (WS-Man port + 2) |
| SOL: settings exchange, send/receive, `--interactive` | works against the mock; on hardware the device closes after the start request until SOL/IDER is enabled in MEBx |
| KVM video: RFB handshake, RAW/RLE decoders, `--screenshot` | **works on hardware** — 1920x1080, RGB565, real tiles decoded |
| KVM video decoder | **works on hardware** — live capture of the host console, text legible, colours correct |
| KVM window with keyboard and mouse (`kvm` without `--screenshot`) | **opens and streams on hardware** — winit 0.29 + softbuffer 0.4, keys and mouse forwarded as RFB events; input not yet exercised end to end |
| Boot to BIOS setup (`bios`, `bios --status`, `--cancel`) | **works on hardware** — writes `AMT_BootSettingData.BIOSSetup`, verified by reading it back |
| IDER (virtual CD/floppy) | not implemented |

### What still bites

- **The redirection service accepts one client at a time.** While a
  MeshCommander session is open (even an abandoned one) every connect gets
  `StartRedirectionSession refused, status 2`. Stop the other client and wait
  for the session to time out.
- **The firmware encodes every tile that contains text with "palette RLE"**
  (`subencoding > 129`): `subencoding - 128` RGB565 palette entries, then one
  index byte per run, where an index above 127 means the run length follows
  (255 continues). Missing this branch is what made text come out as solid
  blocks: those tiles were skipped, leaving only the solid background tiles.
  The branch is now implemented in `decoder::draw_rle_tiles`.
- **Keep asking for updates.** The device answers the first request with a
  DesktopSize rect and nothing else; a client that only re-requests after
  painting something stays black forever. Ask again after every update, drawn
  or not, and again after an idle timeout.
- **Do not let the redraw request feed itself**: winit repaints in a tight loop
  (tens of thousands of redraws per second) unless the event loop waits on a
  timer. The window polls the frame channel at 60 Hz with `ControlFlow::WaitUntil`.
- **`amt-cli replay <dump-dir>`** rebuilds a PNG from tiles captured with
  `AMT_DUMP`, which is how the decoder was fixed without a live screen.
- **On macOS the winit event loop must own the main thread.** Creating it on a
  worker thread panics with "EventLoop must be created on the main thread", so
  the RFB session runs on its own thread with its own tokio runtime and the
  window runs on main. `screencapture` needs Screen Recording permission, which
  is why the window was verified by process state rather than pixel capture.
- **Firmware flushes the deflate stream instead of closing it**, so a
  `read_to_end` zlib decoder fails with "incomplete deflate stream"; use
  `decoder::inflate_partial`. Tiles are always packed as full 64x64 blocks,
  even at the right and bottom edges: consume 4096 pixels regardless of the
  rectangle size or the next tile starts mid-stream.
- **A blanked console sends nothing at all.** The device answers the first
  update with a single `1920x1080` DesktopSize rect and then goes quiet: no
  tiles, no pixels. `kvm --key shift` re-sends the key every 1.5s, which is the
  right wire format (`[4, down, 0, 0, keysym_be32]`, the same bytes
  MeshCommander sends) but on this firmware it did not bring the console back.
  Touch the physical console and the next capture has content again.
- **`AMT_DUMP=<dir>`** writes every RLE block and its inflated tile stream to
  disk, so the decoder can be fixed offline instead of guessing from a PNG.

## Layout

```
amt-rs/
├── crates/
│   ├── amt-wsman/   # WS-Man client: SOAP envelope, Digest auth, CIM types
│   ├── amt-redir/   # AMT redirection protocol (binary, TCP/TLS): handshake, KVM, SOL, IDER
│   ├── amt-kvm/     # video decoder (RLE8/RLE16/RAW/ZLIB) and input encoder
│   └── amt-cli/     # the `amt-cli` binary
└── config.example.yaml
```

## The envelope that this firmware accepts

This is the part that costs time; everything else is ordinary code. Intel
AMT's WS-Man validator answers `400 Bad Request` with
`SchemaValidationError` for anything it does not like, without saying
which element failed, so an envelope that "looks right" is not enough.
What works here:

- `https` only, port **16993**. On this firmware 16992 is closed and
  plain HTTP is not served at all.
- **No** `<w:OptionSet/>`, **no** `<w:OperationTimeout>`. Adding them
  (which several public templates do) is rejected.
- `ManagedElement` must be a complete EPR — `Address` plus
  `ReferenceParameters` with `ResourceURI` for `CIM_ComputerSystem` and a
  `SelectorSet` carrying `CreationClassName`/`Name=ManagedSystem`. A bare
  reference is rejected.
- The body must be a single line; nothing (not even whitespace or a BOM)
  before `<?xml`.
- `Content-Type: application/soap+xml;charset=UTF-8`, Digest auth over TLS,
  self-signed certificate accepted.

The request that the AMT here accepts for a power state change
(`PowerState`: 2=on, 8=off-soft, 5=power cycle; the device also reports
11=reset as available):

```xml
<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:w="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"><s:Header><a:Action s:mustUnderstand="true">http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService/RequestPowerStateChange</a:Action><a:ReplyTo><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address></a:ReplyTo><a:MessageID>uuid:00000000-0000-4000-8000-000000000000</a:MessageID><a:To s:mustUnderstand="true">https://192.168.99.153:16993/wsman</a:To><w:ResourceURI s:mustUnderstand="true">http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService</w:ResourceURI><w:SelectorSet><w:Selector Name="CreationClassName">CIM_PowerManagementService</w:Selector><w:Selector Name="Name">Intel(r) AMT Power Management Service</w:Selector><w:Selector Name="SystemCreationClassName">CIM_ComputerSystem</w:Selector><w:Selector Name="SystemName">Intel(r) AMT</w:Selector></w:SelectorSet></s:Header><s:Body><p:RequestPowerStateChange_INPUT xmlns:p="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService"><p:PowerState>2</p:PowerState><p:ManagedElement xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:w="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address><a:ReferenceParameters><w:ResourceURI>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ComputerSystem</w:ResourceURI><w:SelectorSet><w:Selector Name="CreationClassName">CIM_ComputerSystem</w:Selector><w:Selector Name="Name">ManagedSystem</w:Selector></w:SelectorSet></a:ReferenceParameters></p:ManagedElement></p:RequestPowerStateChange_INPUT></s:Body></s:Envelope>
```

The response is a plain `RequestPowerStateChange_OUTPUT` with
`<g:ReturnValue>0</g:ReturnValue>` on success — and that value, not the
HTTP status, is what the client checks.

## Build and run

Step-by-step instructions, including what to look at and what to collect when
something goes wrong, are in [docs/testing.md](docs/testing.md).

```sh
cargo build --release
./target/release/amt-cli power                    # query the current state
./target/release/amt-cli power --state on
./target/release/amt-cli sol --seconds 10         # print the console for 10 s
./target/release/amt-cli sol --interactive        # type to the console
./target/release/amt-cli kvm --screenshot out.png # decode one full frame
```

Credentials come from `~/.amt/config.yaml` or from `AMT_HOST`, `AMT_PORT`,
`AMT_USER`, `AMT_PASS`.

Redirection listens two ports above WS-Man: **16993 → 16995** with TLS
(16994 is the plain-text variant, used with 16992). Verified on hardware.

## Configuration

`~/.amt/config.yaml`, mode `0600`, never committed (see
`config.example.yaml` and `.gitignore`):

```yaml
amt:
  host: 192.168.99.153
  port: 16993
  username: admin
  password: CHANGE_ME
```

There is no environment-variable override yet; if you need one, that is
a small change in `crates/amt-cli/src/config/mod.rs`.

## Testing against a live host

`PowerState 2` (on) against an already-running machine is a no-op: that
is the safe acceptance test, together with the `Get` read that returns
the current `PowerState`/`RequestedPowerState`. Never test `off`,
`reset` or power cycle on a machine whose availability matters.

A quick independent check of network + credentials + envelope, without
the Rust client, is a `curl --digest -k` POST of the XML above to
`https://<host>:16993/wsman`: a `200` with `ReturnValue 0` means
everything except the client is fine.

`AMT_DEBUG_XML=1` makes the client print the exact SOAP request it sends
(on stderr). Diffing that against the template above is the fastest way
to find a broken envelope — a stray backslash or an inserted newline is
enough to get `SchemaValidationError`.

## Roadmap

1. KVM window (winit + softbuffer) with keyboard and mouse input — the frame
   pipeline, decoders and input encoding are the missing glue.
2. Dirty-rectangle updates instead of a full repaint per frame.
3. IDER (virtual CD/floppy), the last piece before this can replace
   MeshCommander for real work.

## License

GPL-3.0, like the parent repository. MeshCommander (Apache-2.0) is used
as a protocol reference only; no code from it has been copied into this
tree. Anything ported from it later must be marked as such and carries
its Apache-2.0 terms.
