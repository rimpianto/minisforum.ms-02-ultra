# amt-rs — a native Intel AMT client

Rust implementation of the AMT operations we need on the Minisforum MS-02
Ultra: power control, KVM with working keyboard and mouse, boot to BIOS setup,
and a control console beside the window. One static binary, no Node.js
runtime, no browser.

MeshCommander stays the reference implementation - this client was written by
watching what it puts on the wire - but it needs a Node.js process and its
console is a browser page. The wire protocols are documented and small enough
to own.

## State

**1.1.3** — everything below was exercised against the AMT of this host
(dedicated management LAN, `192.168.99.153`), not only against the mock.

| Part | State |
|---|---|
| Power: `on` (2), `off` (8), `reset` (10), `power cycle` (5) | **works on hardware**, `ReturnValue` validated (a 2xx is not success) |
| Power-state query with names | **works** — `PowerState=8 (Off-Soft) RequestedPowerState=8 (Off-Soft)` |
| Redirection transport + AAAH handshake | **works** — TLS on `16995` (WS-Man port + 2) |
| KVM video, RAW / RLE / palette-RLE decoders | **works** — 1920x1080 RGB565, text legible, colours correct |
| KVM window (winit + softbuffer) | **works** — scales to the window with integer factors, follows resolution changes (POST at 1024x768, desktop at 1920x1080), reconnects by itself |
| Keyboard and mouse | **works** — physical-key mapping, modifiers held so `ctrl+c` and `ctrl+alt+f2` work |
| Control console in the launching terminal | **works** — `status`, `bios`, `boot-bios`, `on`, `off`, `reset`, `cycle`, `machines`, `use <name>`, `reconnect`, `quit`; history with arrows, colours on a TTY |
| Boot to BIOS setup | **works** — writes `AMT_BootSettingData.BIOSSetup` and activates the boot configuration with `SetBootConfigRole`; the machine came up in setup without touching Delete |
| SOL | settings exchange and I/O work against the mock; on hardware the device closes after the start request until SOL/IDER is enabled in MEBx |
| IDER (virtual CD/floppy) | not implemented |

### What still bites

- **One redirection client at a time.** While another KVM (MeshCommander, a
  stale window) holds the session every connect gets
  `StartRedirectionSession refused, status 2`.
- **Power off and power cycle are refused while a client is attached**
  (`ReturnValue 2`). `off` and `cycle` therefore release the session first,
  wait for the socket to close, and retry.
- **`6` is not a valid power state here**: power down is `8`, power cycle is
  `5`, reset is `10`. Sending 6 earns a `ReturnValue 2` that looks like a
  session problem and is not.
- **An AMT that is not there is not retried forever**: after three failed
  connections in a row the session thread stops trying and says so, keeping
  the window and the console alive; `reconnect` (or `use <name>`) starts it
  again. Before this, an unreachable device filled the terminal until the
  run expired.
- **A power cycle is a hardware action**: the OS gets no ACPI shutdown, so
  neither dmesg nor journal shows an orderly shutdown. Use `cycle` if the
  machine must come back on its own, `off` only when you can power it on from
  somewhere that survives the machine being down.
- **The firmware encodes every tile containing text with palette RLE**
  (`subencoding > 129`): `subencoding - 128` RGB565 palette entries, then one
  index byte per run, where an index above 127 means the run length follows
  (255 continues). Without that branch text renders as solid blocks.
- **The device flushes the deflate stream** instead of closing it, so
  `read_to_end` fails; tiles are always packed as full 64x64 blocks even at the
  edges.
- **`AMT_BootSettingData` lives in the Intel namespace**
  (`intel.com/wbem/wscim/1/amt-schema/1`), is a singleton addressed with no
  selectors, and a Put needs the whole instance back minus the read-only
  properties, or the firmware answers `SchemaValidationError`.
- **On macOS the winit event loop must own the main thread**; the session runs
  on its own thread with its own tokio runtime.

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

Credentials come from `~/.amt/config.yaml`, where any number of machines can be
listed, or from `AMT_HOST`, `AMT_PORT`, `AMT_USER`, `AMT_PASS`.

Redirection listens two ports above WS-Man: **16993 → 16995** with TLS
(16994 is the plain-text variant, used with 16992). Verified on hardware.
`redirection_port:` in the config overrides that per machine.

## Configuration

`~/.config/amt/config.yaml` (or `$XDG_CONFIG_HOME/amt/config.yaml`), mode
`0600`, never committed (see `config.example.yaml` and `.gitignore`). The old
location `~/.amt/config.yaml` is still read, and `AMT_CONFIG=/path/file.yaml`
overrides both. As many machines as you like, one at a time:

```yaml
default: ms02                 # what a bare command talks to
machines:
  ms02:
    host: 192.168.99.153      # AMT of the Minisforum MS-02 Ultra
    port: 16993               # WS-Man over TLS
    username: admin
    password: CHANGE_ME
  lab:
    host: 192.168.99.160
    port: 16993
    username: admin
    password: CHANGE_ME
    redirection_port: 16995   # optional; port + 2 when omitted
```

`mac:` is optional and never used on the wire: it is there so `machines`
output can be matched against a DHCP lease table.

```sh
./target/release/amt-cli machines             # what is configured, * = default
./target/release/amt-cli --machine lab power  # one command on another machine
./target/release/amt-cli --machine lab kvm    # console on another machine
```

Inside the KVM console `machines` lists them and `use <name>` reattaches the
same window to another one, without closing it.

The old single-machine form still loads, so an existing config does not break:

```yaml
amt:
  host: 192.168.99.153
  port: 16993
  username: admin
  password: CHANGE_ME
```

Environment overrides, applied to the machine that was chosen: `AMT_HOST`,
`AMT_PORT`, `AMT_USER`, `AMT_PASS`, `AMT_REDIR_PORT`, and `AMT_MACHINE` to
choose the machine. With `AMT_HOST` set and no file at all the tool works as
before: `AMT_HOST=192.168.99.153 AMT_PASS=... amt-cli power`.

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

The list, with the reason each item matters and what "done" means, is in
[TODO.md](TODO.md). In short, next is dirty-rectangle updates, then IDER, then
the clipboard over the KVM data channel.

Versions up to 1.0.0 followed the commit count (`0.1.<n>`, written into the
manifest by `scripts/sync-version.sh`). From 1.0.0 the version is a human
decision.

## License

GPL-3.0, like the parent repository. MeshCommander (Apache-2.0) is used
as a protocol reference only; no code from it has been copied into this
tree. Anything ported from it later must be marked as such and carries
its Apache-2.0 terms.
