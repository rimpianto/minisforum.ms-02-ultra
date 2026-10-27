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

Either a config file at `~/.config/amt/config.yaml` (or `$XDG_CONFIG_HOME/amt/config.yaml`;
`~/.amt/config.yaml` still works, and `AMT_CONFIG` points anywhere), which can
hold several machines (one is used at a time):

```yaml
default: ms02

machines:
  ms02:
    host: 192.168.99.153
    port: 16993
    username: admin
    password: CHANGE_ME
  lab:
    host: 192.168.99.160
    port: 16993
    username: admin
    password: CHANGE_ME
```

```sh
amt-cli machines                  # what is configured, * marks the default
amt-cli --machine lab power       # the same, against another machine
```

The single-machine form (`amt:` with `host`, `port`, `username`, `password`)
still loads. Inside the KVM window `machines` lists them and `use <name>`
reattaches the window to another one.

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

## 5c. Drive the machine from the terminal, watch it in the window

With the window open the session reconnects by itself, because a reset kills
it and the interesting part (POST, setup) comes afterwards. The terminal that
launched it stays usable as a small control console:

```
commands: status | bios | bios status | bios cancel | boot-bios
          on | off | reset | cycle | quit          (help for this list)
```

Type `bios`, then `reset`, and watch the window: the session drops, comes back
by itself, and you are looking at the firmware. No Delete-key reflexes.

Colours appear only on a terminal (`NO_COLOR` and `TERM=dumb` turn them off),
and the arrows recall what you typed: history lives in `~/.amt/history`.

`off` and `cycle` release the console first, because the firmware refuses a
power off while a client is attached; the window then reattaches by itself, so
`off` followed later by `on` brings the picture back without reopening
anything.

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

The window follows the framebuffer resolution: POST and setup run at 640x480 or
1024x768 while the desktop is 1920x1080, and small screens are enlarged by
whole factors only - a fractional nearest-neighbour upscale is what makes text
look ragged.

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

`off` deserves one more thought on a machine that is itself the site router (a
router card inside the host): powering it off takes that card, and everything
behind it, down with the host. Whether that matters depends on where the AMT
sits. If its port is wired to a dedicated management link, as on our host (a
2.5GbE link straight to the management network, while the 10GbE link goes to
the router the card also uses), the AMT stays reachable with the host off and
`power --state on` brings it back. If instead the path to the AMT crosses the
card, `off` strands the machine: nothing would be able to power it on again.
`cycle` is safe either way, because the firmware performs the off/on itself.

