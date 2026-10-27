# Trying it out

What you need: a Rust toolchain, this repository, and network access to the
AMT interface (management LAN, `192.168.99.153` on our host).

## 1. Build

```
cd minisforum.ms-02-ultra/amt-rs
cargo build --release
```

The binary is `target/release/amt-cli`.

## 2. Point it at the device

Either a config file at `~/.amt/config.yaml`:

```yaml
amt:
  host: 192.168.99.153
  port: 16993
  username: admin
  password: CHANGE_ME
```

or environment variables, which win over the file:

```
export AMT_HOST=192.168.99.153
export AMT_PORT=16993
export AMT_USER=admin
export AMT_PASS='...'
```

The redirection port is derived as `port + 2` (16993 → 16995). Override with
`AMT_REDIR_PORT` if the firmware disagrees.

## 3. Free the KVM session

The device serves one redirection client at a time. Close the remote desktop
window in MeshCommander (closing the app is not required) or any other KVM
client, otherwise every connection ends with:

```
Error: StartRedirectionSession refused, status 2
```

## 4. Read the power state (harmless)

```
./target/release/amt-cli power
```

Prints `PowerState=N (name)`. With no `--state` it only reads.

## 5. Non-interactive capture

```
./target/release/amt-cli kvm --seconds 10 --screenshot /tmp/screen.png
```

Opens the session, saves one full frame, exits. Useful to check the video path
without any GUI. A completely black PNG usually means the host console is
blanked (DPMS): touch the physical keyboard and retry, or add `--key shift`.

## 5b. Boot straight into the BIOS setup

Hitting Delete during POST is a reflex test: the KVM session takes seconds to
come up and the window is a few hundred milliseconds wide. The firmware can be
asked to stop in setup instead.

```
./target/release/amt-cli bios --status     # is a setup boot pending?
./target/release/amt-cli bios              # ask for it on the next boot
./target/release/amt-cli bios --reset      # ask, then reset now
./target/release/amt-cli bios --cancel     # change your mind
```

The flag is stored in `AMT_BootSettingData.BIOSSetup` and the firmware clears
it once it has been used, so it cannot get stuck. `--reset` while a KVM session
is open is fine: the AMT rejects power off and power cycle during redirection,
but a reset goes through, and the session stays attached so you see POST.

## 6. The window

```
./target/release/amt-cli kvm --seconds 120
```

A window titled `amt-rs KVM` at the host resolution. While it is open:

- typing in the window should appear on the host console;
- moving the cursor inside the window should move the host pointer;
- the session ends after `--seconds`, or when you close the window.

What is worth checking, in order: text legible and colours right, keys
reaching the host, pointer reaching the host, window not tearing or freezing.

On macOS the window cannot be screenshot by a background shell unless that
shell has the Screen Recording permission; verify by eye.

## 7. If something is wrong, capture the evidence

```
AMT_DUMP=/tmp/tiles ./target/release/amt-cli kvm --seconds 10 --screenshot /tmp/s.png
./target/release/amt-cli replay /tmp/tiles --out /tmp/replay.png
```

`AMT_DUMP` stores every RLE block plus its inflated tile stream; `replay`
rebuilds a PNG from them offline, which is how the decoder is debugged without
depending on the host's screen state.

## Do not

Do not run `power --state off` or `power --state reset` on a host you care
about: this client has no confirmation prompt.
