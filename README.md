# Candeo

> *candeo*, Latin verb — "I shine, I glow". The root of *candela*,
> the SI unit of luminous intensity.

Keyboard lighting control, through **direct HID access**. No vendor runtime,
no third-party service, no abstraction layer: the application talks to the
device.

---

## Status

Working proof of concept on the **Razer DeathStalker V2 Pro (wired)**.
The protocol was surveyed by capturing the USB bus, then **validated by direct
writes** — solid colors, per-row gradient, partial row writes and firmware
effect switching, all with no third-party software running.

| Layer | Status |
|---|---|
| `candeo-protocol` — reports and checksum | done, tested against a captured frame |
| `candeo-device` — HID transport and layouts | done, verified on hardware |
| Tauri commands | wired and documented — see [`docs/api/`](docs/api/commands.md) |
| Vue interface | three screens — library, devices, editor; decisions in [`docs/design/`](docs/design/studio.md) |
| User effects engine | single `rquickjs` engine on the Rust side; storage, settings and **five shipped effects** done |

---

## Install

Each [release](https://github.com/oorabona/candeo/releases) carries the
installers and a `SHA256SUMS` file.

**Windows (x64)**

- `candeo_<version>_x64-setup.exe` installs for the current account, without
  administrator rights. The one to pick.
- `candeo_<version>_x64_en-US.msi` installs for every account, and needs
  administrator rights.
- The installers are not signed yet: on first run, SmartScreen warns about an
  unknown publisher. **More info**, then **Run anyway**.

**Linux (x86_64)**

- Debian, Ubuntu: `sudo apt install ./candeo_<version>_amd64.deb`
- Fedora: `sudo dnf install ./candeo-<version>-1.x86_64.rpm`
- Both packages install the [udev rule](#udev-rule) that gives the signed-in
  user access to the keyboard: plug it in again after installing. There is no
  AppImage, which cannot install that rule.

**Checking a download**: `sha256sum -c SHA256SUMS --ignore-missing` on Linux;
on Windows, `Get-FileHash <file>` in PowerShell, compared with its line in
`SHA256SUMS`.

Updating means installing the next release over the current one: settings and
effects are kept.

---

## Protocol origin

The protocol was established **by observing the hardware**: Windows PnP
enumeration, querying an SDK server over its network protocol, capturing the USB
bus with USBPcap and Wireshark, then direct writes and reads through the HID API.

Each fact carries the date it was established and **the firmware version it was
established against** — v1.5, on 11 and 12/09/2026. That is what makes it
possible to diagnose unexpected behavior.

Facts about a protocol are not subject to copyright, and surveying them for
interoperability purposes is provided for by **Article L.122-6-1 IV of the
French Intellectual Property Code** (transposing Directive 2009/24/EC,
Article 6).

The full protocol documentation, with annotated frames and the reproducible
capture method, is in [`docs/protocol/`](docs/protocol/).

---

## Structure

```
candeo/
├── crates/
│   ├── candeo-protocol/   construction des rapports — pur, sans I/O, testable
│   └── candeo-device/     transport HID et gabarits de périphériques
├── apps/
│   └── desktop/           application Tauri (Vue 3 + TypeScript)
│       └── src-tauri/     liaison Rust, boucle de rendu
├── packages/
│   └── effects-api/       types TypeScript pour l'écriture d'effets
├── packaging/
│   └── linux/             règle udev, livrée par les paquets deb et rpm
└── docs/
    ├── protocol/          relevé du protocole et méthode de capture
    ├── api/               commandes exposées au front
    └── design/            décisions d'interface et d'exécution, prises avant code
```

The `protocol` / `device` split is deliberate: report construction and the
checksum are tested **without hardware**, in continuous integration.

---

## Interface design

Decisions are frozen before any Vue is written, and documented in
[`docs/design/studio.md`](docs/design/studio.md). Reference mockup:
not published

The three design choices that structure everything else:

- **The effect gallery is the home screen**, not the editor. A "＋" enters
  editing; most launches are for choosing, not for writing.
- **A single engine runs effects**, `rquickjs` on the Rust side, in a thread
  independent of the window. The front end never runs user code: it sends the
  source and receives the frames. The preview therefore **is** production, and
  an effect keeps running with the window closed. The motive was never
  performance — 132 LEDs × 30 fps = 3,960 colors/s, trivial.
- **Closing the window tucks Candeo into the system tray.** This is the second
  half of the previous sentence, and it did not hold until issue #46: a thread
  independent of the window does not outlive the process, and the process exited
  with its last window. The tray icon keeps it alive — it intercepts
  `RunEvent::ExitRequested` — and provides what is needed to control things
  without the window: current effect and library per controlled device, sending
  to the keyboard as a toggle, lights off. **"Quit Candeo" is
  the only real exit there**, and quitting leaves the lighting as it is. See
  [`src/tray.rs`](apps/desktop/src-tauri/src/tray.rs).
- **The simulator draws the real keyboard**, full-size ISO layout. The device
  only declares its 6 × 22 logical grid; the physical geometry is written by
  hand.

The full lifecycle of an effect — transpilation, storage, render loop, frames
flowing back — is described in
[`docs/design/effects-runtime.md`](docs/design/effects-runtime.md).

---

## Portability

Developed on Windows, written so as not to be locked into it. Nobody has a Linux
machine in the loop, so CI plays that role: the `linux` job builds the full
workspace on `ubuntu-latest`, runs the tests, packages as `.deb` and `.rpm`,
and **reads the produced packages** to check that the udev rule is in them.

Hence the distinction this section keeps: what is **compiled** and what remains
**assumed**.

### Compiled and verified

- **Hardware access.** The `hidraw` backend of `hidapi` builds on Linux, and
  nothing in the three crates depends on Windows to compile.
- **Packaging.** `.deb` and `.rpm` are produced, and the udev rule is present
  in both at `/usr/lib/udev/rules.d/60-candeo.rules` — verified by reading the
  packages, not by rereading the configuration. ⚠️ **This check no longer runs
  on pull requests**, only on `main` and on demand: see
  [Verifying Linux locally](#verifying-linux-locally).
- **Protocol.** `candeo-protocol` has no system dependencies: it builds bytes,
  and its tests run everywhere.
- **Paths.** No path is hard-coded: Tauri's API applies the system's convention.
  Shipped effects go in `app_data_dir()`
  (`%APPDATA%\com.o2csi.candeo` or `~/.local/share/…`), the user's in
  `Documents/candeo/effects`, settings in `app_config_dir()`. **Never in `Program Files`** — read-only for a standard
  account, and shared by all accounts.

### Assumed, for lack of Linux hardware

There is no USB behind a GitHub runner. Three points are waiting for a keyboard
plugged into a Linux machine: that feature report writes go through over hidraw,
that `interface_number` tells the composite device's interfaces apart as it does
on Windows, and that the folders resolved by Tauri really do land in
`~/.local/share` and `~/.config`.

### Verifying Linux locally

The CI `linux` job takes a quarter of an hour, about ten minutes of which go into
recompressing a debug `.rpm`. So it does **not** run on pull requests: only on
`main`, and on demand via **Actions → CI → Run workflow**, picking the branch.

In the meantime, WSL does the same work without waiting for a runner:

```bash
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev \
  libudev-dev build-essential pkg-config file rpm

cargo check --workspace --all-targets
cargo test --workspace
```

And if the change touches packaging, HID I/O or system dependencies, the full
check — this is the slow step, to run only in that case:

```bash
pnpm install --frozen-lockfile
pnpm --filter @candeo/desktop exec tauri build --debug --bundles deb,rpm

regle='usr/lib/udev/rules.d/60-candeo.rules'
dpkg-deb -c "$(ls target/debug/bundle/deb/*.deb | head -1)" | grep -F "$regle"
rpm -qpl "$(ls target/debug/bundle/rpm/*.rpm | head -1)" | grep -F "/$regle"
```

The two `grep` calls are the proof: the rule really is **in the packages**, not
merely declared in `tauri.conf.json`.

### udev rule

`/dev/hidraw*` is created as `0600 root:root`. The file is in
[`packaging/linux/`](packaging/linux/60-candeo.rules); the `deb` and
`rpm` packages install it. From source, you have to copy it yourself:

```bash
sudo cp packaging/linux/60-candeo.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

The details — why `uaccess` rather than a group, why the 60 prefix, why the
`*-native` backends of `hidapi` were evaluated and then set aside — are in
[`docs/design/effects-runtime.md`](docs/design/effects-runtime.md) §6.

---

## The protocol at a glance

USB control transfer, `SET_REPORT` on a feature report, interface 3.

```
bmRequestType 0x21   bRequest 0x09   wValue 0x0300   wIndex 3   wLength 90
```

90-byte report:

| Offset | Content |
|---|---|
| 1 | transaction ID (`0x9f`) |
| 5 | argument size |
| 6 | class — `0x0f` = lighting |
| 7 | command — `0x02` effect, `0x03` row, `0x04` brightness |
| 8+ | arguments |
| 88 | **XOR of bytes 2 to 87** |

Colors in **RGB** — not to be confused with the Chroma SDK, which uses `0x00BBGGRR`.

---

## Writing an effect

An effect is a function from time and position to a color. That is precisely
why **YAML does not fit**: it would describe a configuration, not a behavior.

```ts
import { defineEffect, hsv } from '@candeo/effects-api'

// Un export par défaut, et rien d'autre : c'est tout ce que le moteur cherche.
// `defineEffect` ne fait rien à l'exécution — elle donne un type contextuel,
// ce qui type `layout`, `time`, `frame`, et `params` d'après sa déclaration.
export default defineEffect({
  name: 'Onde',
  params: {
    speed: { kind: 'number', label: 'Vitesse', min: 0, max: 400, default: 120 },
  },
  render({ layout, time, frame, params }) {
    const cx = (layout.cols - 1) / 2
    const cy = (layout.rows - 1) / 2
    // `layout.keys` : les 106 positions éclairées, pas les 132 cases.
    for (const key of layout.keys) {
      const d = Math.hypot(key.col - cx, key.row - cy)
      // `params.speed` est un `number`, déduit de sa déclaration ci-dessus.
      frame.set(key, hsv(time * params.speed + d * 18, 1, 1))
    }
  },
})
```

The effects shipped with the application — Radial wave, Diagonal wave,
Breathing, Sweep, Fixed gradient, Color wheel, Noise map, Rain, Starry night,
Bubbles, Lightning, Crossing beams, Swirl circles, Ripples — are written against
this same API, in
[`packages/effects/`](packages/effects/). They are copied into your effects folder
at first launch and kept up to date. The application does not change them:
Duplicate one to make it yours, and Restore brings back one changed or removed in
the folder.

An effect can react to the keys you press: Ripples spreads a ring from each one.
Key presses are read only while such an effect runs, as key positions, never as
characters, and are neither logged nor kept beyond a few seconds
([`docs/design/key-input.md`](docs/design/key-input.md)). Windows only for now.

---

## Development

```bash
pnpm install
pnpm dev            # application en mode developpement
pnpm check          # clippy + tests Rust
cargo test -p candeo-protocol   # tests du protocole, sans materiel
```

### Prerequisites

Stable Rust, Node 22+, pnpm 10+.

On **Windows**, the WebView2 runtime — present by default on Windows 11.

On **Debian / Ubuntu**, the Tauri 2 system dependencies, plus `libudev-dev`,
which `hidapi` resolves through `pkg-config`:

```bash
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev \
  libudev-dev build-essential pkg-config file
```

This is the list the CI `linux` job installs — if it goes stale, CI will say so.
The job adds `rpm`, which is only used to read back the produced package.

---

## Pitfalls encountered, for the record

- **132 and 106 are not the same thing.** The matrix is 6 × 22 = **132**
  cells, and that is what a frame must cover; only **106** of them carry a key.
  Sending 106 leaves the last rows frozen at their previous value — symptom
  actually seen: the bottom row stuck on white.
- The device's variant string **changes with the firmware**
  (`v1.4 / Unkown Variant` → `v1.5 / Quartz`). Identify by VID / PID / serial number.
- The `HidD_SetFeature` buffer is **91 bytes**: report ID, then the 90 bytes of
  the report.
- On a USB composite device, opening the wrong interface gives a valid handle on
  which every write fails.

---

## Issue history

The project was developed in a private repository before being published. Its
issues and pull requests have been recreated here **under their original
numbers**, so that the `#N` references in commit messages, code and
documentation lead to the right place. A pull request appears here as a closed
issue, labeled `PR archivée` (archived PR), that links to its commit in the
history of `main`.

## License

Distributed under the terms of the GNU General Public License, **version 3
only** (`GPL-3.0-only`). See [`LICENSE`](LICENSE).
