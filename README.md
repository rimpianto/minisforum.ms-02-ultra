# Minisforum MS-02 Ultra — remote-node operation notes

Practical notes for running a **Minisforum MS-02 Ultra** (Arrow Lake-HX)
as an unattended Proxmox VE host, with a MikroTik CCR2004-1G-2XS-PCIe
router card installed. Everything here was validated on real hardware
in September 2026; the companion card-specific repository is
[rimpianto/mikrotik.CCR2004-1G-2XS-PCIe](https://github.com/rimpianto/mikrotik.CCR2004-1G-2XS-PCIe).

Contents:

1. [Intel AMT/vPro: making the MS-02 remotely recoverable](#1-intel-amtvpro)
2. [The PCIe fabric freeze (with the CCR2004-PCIe card)](#2-the-pcie-fabric-freeze)
3. [The link-disable workaround (LnkDisable)](#3-the-link-disable-workaround)
4. [Boot hardening: GRUB, panic=, hardware watchdog](#4-boot-hardening)
5. [Validation matrix](#5-validation-matrix)

---

## 1. Intel AMT/vPro

The MS-02 Ultra ships with a working Intel ME (firmware v19.x) and AMT
can be enabled in BIOS. Once configured, the machine is remotely
recoverable even when the OS is completely wedged — which, see below,
is not a theoretical concern.

Setup (BIOS 1.04):

- Advanced → CPU Configuration: disable the `igc` kernel driver claim on
  the I226-LM port used by AMT (`blacklist igc` in `/etc/modprobe.d/`,
  the port is then owned by the ME alone)
- AMT settings: enable AMT, set a strong admin password, enable
  TLS/Digest on port 16993
- Give the AMT port an address on a dedicated management LAN (out-of-band,
  NOT the data plane)

Once up you get:

- **Power control** — on / off / power cycle over WS-Man, works with the
  OS frozen (verified: a platform-wide freeze was recovered with a
  remote power cycle in ~45 seconds)
- **KVM console** — full remote video/keyboard, works even before boot
- **SOL** — serial-over-LAN

A minimal power-control client (~140 lines of Python, WS-Man Digest over
TLS) is enough for `status / on / off / cycle`; MeshCommander works for
interactive use but its KVM is barely usable (no copy-paste, poor
rendering) — a native rewrite is a separate project.

**Design rule:** the AMT port and the card management port must be on a
LAN that does not depend on the host being alive. A small switch and a
management VLAN is enough. If the data plane dies, you still have both
power control and the card's management SSH.

## 2. The PCIe fabric freeze

Summary of the failure mode (full evidence in the card repository):

When the CCR2004-PCIe card resets (RouterOS reboot / upgrade), it
disturbs the PCIe fabric of the MS-02 at a level **below the OS**:

- With the card's PCI functions hot-removed from the bus and the driver
  quiesced — i.e. no software path to the card at all — the platform
  can still freeze completely: no kernel messages, no panic, every NIC
  dead (including ones on different root ports), only the Intel ME
  keeps answering
- Non-deterministic: 1-in-2 identical runs froze in our testing
- The card also never re-enumerates after its own reboot while the host
  stays up; `/sys/bus/pci/rescan` does not bring it back

This is a card-side defect (MikroTik ticket SUP-223678), but the MS-02
is one of the platforms where its blast radius is the whole machine —
which makes the workarounds below worth documenting for this host.

## 3. The link-disable workaround

The glitch travels over the **PCIe link**, not over the functions.
Disabling the link itself before the card resets isolates the platform:

```sh
# find the root port above the card
lspci -t
# in our unit: 00:06.0 (Intel Meteor Lake-H PCIe Root Port), card at 01:00.0-3

# quiesce the driver, remove the functions
ip link set nic0 nomaster
for n in nic0 nic1 nic2 nic5; do ip link set $n down; done
sleep 3
for f in 0 1 2 3; do echo 1 > /sys/bus/pci/devices/0000:01:00.$f/remove; done

# THE key step: bring the link down (LnkCtl bit 4, PCIe spec)
setpci -s 00:06.0 CAP_EXP+10.w   # read current, e.g. 0c40
setpci -s 00:06.0 CAP_EXP+10.w=0c50   # set bit 4 (0x10)
setpci -s 00:06.0 CAP_EXP+12.w   # verify: bit 13 (0x4000) must now be 0 = DLActive down

# now reboot the card over its management Ethernet; the host stays alive
```

Known limits (all verified):

- The Intel root port does **not** re-train the link when LnkDisable is
  cleared at runtime — the card comes back only after a host warm reboot
- The DMA engine of the host NICs stays wedged after the card reset,
  so a warm reboot is required at the end of the procedure regardless

The full automation script (`ccr-linksafe-reboot`, optionally fully
unattended with `--auto`) is in the card repository. Net effect: card
reboot with **zero risk of platform freeze**, at the price of one
planned host warm reboot (~6 minutes total).

**For other motherboards:** the same mechanism (LnkDisable on the root
port) is generic PCIe and should work wherever the root port implements
the bit; the link re-training limitation and the final warm reboot
should be assumed until proven otherwise. If your board does not allow
this, only the boot hardening below (plus out-of-band power control)
stands between a card reset and an unattended host that never comes
back.

## 4. Boot hardening

Three independent layers that guarantee "the machine always comes
back by itself". These are worth having on any unattended host, and
they specifically cover the three ways this machine can die:

```sh
# (a) GRUB: never sit at the menu for long after an unclean shutdown
echo 'GRUB_RECORDFAIL_TIMEOUT=5' >> /etc/default/grub

# (b) kernel: auto-reboot 10s after a panic
echo 'GRUB_CMDLINE_LINUX="$GRUB_CMDLINE_LINUX panic=10"' \
    > /etc/default/grub.d/panic-reboot.cfg

# NOTE for Proxmox with proxmox-boot-tool (ZFS/systemd ESP sync):
# /etc/kernel/cmdline and /boot/grub/grub.cfg are NOT what the firmware
# reads. After changing /etc/default/grub* run:
proxmox-boot-tool refresh
# and verify on the ESP itself, e.g.:
#   mount /dev/disk/by-uuid/<esp-uuid> /mnt && grep panic /mnt/grub/grub.cfg

# (c) hardware watchdog: the Intel TCO (the same silicon AMT lives in)
#     reboots the machine if the OS freezes silently
sed -i 's/^#*RuntimeWatchdogSec=.*/RuntimeWatchdogSec=30/' /etc/systemd/system.conf
sed -i 's/^#*RebootWatchdogSec=.*/RebootWatchdogSec=10/' /etc/systemd/system.conf
```

Why each one matters here specifically:

| Layer | Catches | Why it's needed on this host |
|---|---|---|
| GRUB recordfail timeout | hanging at the boot menu | unclean shutdowns are routine here (freeze → power cycle) |
| `panic=10` | a kernel that panics and hangs | without it, a panic = physically visit the machine |
| `RuntimeWatchdogSec=30` | **the silent fabric freeze** | hardware reset independent of the OS; the freeze leaves zero log and kills everything except the ME |

The watchdog layer is the important one: it turns "host found dead
days later" into "host was down for 3 minutes". Combined with the
link-disable procedure (which prevents the freeze in the first place),
the machine survived a full card-reboot cycle unattended with zero
human intervention.

## 5. Validation matrix

All on BIOS 1.04, Proxmox VE (kernel 7.0.14-19-pve), card on RouterOS
7.24.4, September 27 2026:

| Test | Link state during card reset | Host outcome |
|---|---|---|
| Card reset, driver active | up | immediate wedge (soft lockup, fixed upstream by atl1c `tpd_cons` patch) |
| Card reset, functions removed (no LnkDisable) | up | freeze 1-in-2 runs, silent, below OS level |
| Card reset, link disabled | **down** | survived 4/4, kernel alive throughout |
| Fully unattended cycle (`--auto`) | down | host back in ~6 min, zero intervention |
| AMT power cycle after a freeze | — | full recovery in ~45 s |

---

Feedback welcome via issues. Card-side bug details and the driver
patches live in
[rimpianto/mikrotik.CCR2004-1G-2XS-PCIe](https://github.com/rimpianto/mikrotik.CCR2004-1G-2XS-PCIe).
