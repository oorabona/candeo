# Lighting protocol — Razer DeathStalker V2 Pro (wired)

**Survey of 11–12/09/2026 · validated by direct writes**

> **Source of the information.** Established by observing the hardware: Windows PnP
> enumeration, querying an SDK server over its network protocol, capture of the USB bus
> (USBPcap 1.5.4.0 + Wireshark 4.6.8), then **direct writes and reads** via
> `HidD_SetFeature` / `HidD_GetFeature`.
>
> **Everything below applies to firmware v1.5**, surveyed on 11 and 12/09/2026.
> The log in §11 dates each fact. Whatever could not be verified on the device
> is marked **unverified**, and remains in §10.
>
> Facts about a protocol are not covered by copyright, and surveying them
> for interoperability purposes is provided for by **article L.122-6-1 IV of the French
> Intellectual Property Code** (Directive 2009/24/EC, article 6).

---

## 1. Identification

| Item | Value |
|---|---|
| Manufacturer | Razer — `VID 0x1532` |
| Product | DeathStalker V2 Pro wired — `PID 0x0292` |
| Serial number | *(masked)* |
| Firmware | `v1.5` |
| Declared variant | `Razer Device, French (ISO), Quartz` |

> ⚠️ **Never identify the device by its variant or its firmware.** The same
> keyboard declared itself `v1.4 / Unkown Variant` in 2024. A binding that matches on these
> fields breaks on update — that is what broke an effect profile during
> the survey. The identity is **VID / PID / serial number**.

### USB interfaces

Composite device with five interfaces. Lighting goes through **`MI_03`**, which the
`wIndex = 3` field of every request confirms.

| Interface | Role |
|---|---|
| `MI_00` | keyboard input + consumer controls |
| `MI_01` | multiple HID collections |
| `MI_02` | HID mouse |
| **`MI_03`** | **lighting** |
| `MI_04` | additional HID input |

> On a composite device, opening the wrong interface gives a **valid** handle on which
> every write fails — with no explicit error. Filter on `interface_number`.

### The `interface -1` entry — it is not the keyboard

HID enumeration carries, on the same VID and PID, an entry **with no interface
number** (`-1`), with no product name, with revision `0x0101` where the whole
composite declares `0x0200`, on usage page `0x000c`/`0x0001` (consumer
control).

Established on 13/09/2026 from the Windows device tree: its path is
`HID#VID_1532&PID_0292&MI_00&Col03&Col01#9&…`, and its **parent** is
`RZVIRTUAL\VID_1532&PID_0292&MI_00&Col03`, a node created by the
`RzDev_0292` service of the manufacturer's driver — itself a child of USB interface `MI_01`.
So it is not a USB interface, hence no number that `hidapi` could
read: it is a **virtual** HID collection, which only exists where that driver is
installed.

Candeo rules it out with the rule that already applies to all the others: the interface
must be the layout's (`Layout::is_lighting_interface`).

---

## 2. Transport

USB **control** transfer, `SET_REPORT` on a **feature** report.

| Setup field | Value |
|---|---|
| `bmRequestType` | `0x21` — host→device, class, interface recipient |
| `bRequest` | `0x09` — `SET_REPORT` |
| `wValue` | `0x0300` — ReportID 0, ReportType Feature (3) |
| `wIndex` | `0x0003` — interface 3 |
| `wLength` | `90` |

### From the Windows HID API

`HidD_SetFeature` expects a **91-byte** buffer: the report ID (`0x00`)
then the 90 bytes of the report. Verified — 90 alone are refused.

```
buf[0]      = 0x00        identifiant de rapport HID
buf[1..91]  = rapport     les 90 octets décrits ci-dessous
```

**Return direction**: `GET_REPORT` — `bRequest 0x01`, `bmRequestType 0xa1`
(device→host), rest of the setup identical. Surveyed through the API, not by
capture: `hid_get_feature_report` (hidapi), `HidD_GetFeature` on Windows,
same 91-byte buffer. What the response contains is described in §8.

---

## 3. Report structure (90 bytes)

```
 offset  taille  contenu
 ------  ------  -----------------------------------------------------
   0       1     ÉTAT                        0x00 en écriture ; porte le sens
                                             dans les RÉPONSES — voir §8
   1       1     identifiant de transaction  observé : 0x9f
   2       2     paquets restants            observé : 0x0000
   4       1     type de protocole           observé : 0x00
   5       1     TAILLE DES ARGUMENTS        varie selon la commande
   6       1     CLASSE                      0x0f = éclairage
   7       1     COMMANDE                    voir §4
   8      N      arguments
  ...       -    remplissage à 0x00
  88       1     SOMME DE CONTRÔLE           XOR des octets 2 à 87
  89       1     réservé                     0x00
```

### Checksum

**XOR of bytes 2 to 87 inclusive**, placed in byte 88. Verified on every
captured frame, across all commands, without exception.

```rust
let crc = report[2..88].iter().fold(0u8, |acc, b| acc ^ b);
```

---

## 4. Command set — class `0x0f`

| Command | Args size | Role |
|---|---|---|
| `0x02` | `0x06`–`0x09` | set the effect |
| `0x03` | `0x47` | write a row of colors |
| `0x04` | `0x03` | set the brightness |
| `0x80` | `0x03` | **read** a descriptor — contains `06 16`, i.e. our 6×22 |
| `0x81` | `0x03` | **read** an enumeration `00`…`09`, meaning not established |
| `0x82` | `0x03` | **read the current effect** — see §8 |
| `0x84` | `0x03` | **read the brightness** |
| `0x86` | `0x03` | **read** `00 01`, meaning not established |

The first two argument bytes are `00 00` in all our captures. A
third-party driver names them *variable storage* and *LED identifier*; we have
**not verified** this — and read responses often start with `05`,
which would be consistent with a "backlight" LED identifier. To be established.

### `0x0f` / `0x04` — brightness

```
args = 00 00 <niveau>
```

Consistently observed at `00 00 ff`. Sent before and after every effect change.

### `0x0f` / `0x02` — effect

```
args = 00 00 <effet> <param1> <param2> 00
```

| Effect | Value | Size | Parameters | Supported |
|---|---|---|---|---|
| Off | `0x00` | `0x06` | — | ✅ |
| **Static** | `0x01` | `0x09` | `args[5]=01`, then R G B | ✅ |
| **Breathing** | `0x02` | `0x09` | `args[5]=01`, then R G B | ✅ |
| Spectrum Cycle | `0x03` | `0x06` | — | ✅ |
| Wave | `0x04` | `0x06` | `param1` direction (`00`–`02`), `param2` speed (obs. `0x28`) | ✅ |
| Reactive | `0x05` | `0x09` | — | ❌ **refused** |
| Starlight | `0x07` | `0x06`+ | — | ❌ **refused** |
| **Direct / custom** | `0x08` | `0x06` | — | ✅ |

**The "supported" column is measured, not inferred**: the effect is set, then
read back through `0x0f`/`0x82` (§8). Identifiers `0x05` and `0x07`, present on
other devices from the brand, leave the effect **unchanged** on this one —
the Wave set just before was still read back identically, parameters included.
`0x06` has not been tried.

> ⚠️ **And yet writing these two refused effects is "accepted": status
> `0x02`.** This is the live demonstration of the danger described in §8 — the device
> validates the class/command pair, **not the value of an argument**. An effect
> identifier is an argument. So no status byte will ever replace a read-back.

> **Correction of an initial reading.** The seventh frame of each update
> cycle is not a commit command: it is `0x02` with effect `0x08`, hence the
> **switch to custom mode**, sent after the rows.

> **Resolved.** `Static` and `Breathing` had produced no frame during the
> capture, and a first sweep had missed them — it set them **with no
> color**, hence in black, which cannot be told apart from a nonexistent effect
> by eye. With `args[5]=01` followed by an RGB triplet, both respond and
> read back.

### `0x0f` / `0x03` — writing a row

```
args = 00 00 <rangée> <col_début> <col_fin>   puis (col_fin - col_début + 1) × (R, G, B)
```

`0x47` = 71 = **5 argument bytes + 66 color bytes** for a full row (22 × 3).

**Partial writes work** — verified on hardware: writing columns 5
to 10 of row 2 affects only those six keys. Useful for localized effects,
which thereby avoid re-sending the whole matrix.

#### Component order: **RGB**

| Color sent | Bytes observed |
|---|---|
| Pure red | `ff 00 00` |
| Pure green | `00 ff 00` |
| Pure blue | `00 00 ff` |

> Not to be confused with the **Chroma SDK**, whose REST API uses `0x00BBGGRR`.
> Assuming one from the other is a mistake.

---

## 5. Sequence of a full update

| # | Command | Content |
|---|---|---|
| 1 → 6 | `0f` / `03` | rows 0 to 5, columns 0→21 |
| 7 | `0f` / `02` | effect `0x08` — switch to custom mode |

A `0f`/`04` (brightness) usually brackets the sequence.

### What this sequence costs — measured on 12/09/2026

**13.1 ms on average, 14.4 ms at worst**, over 120 updates chained as fast
as possible, **without a single refused write** and with the device still responding after
the burst. That is a ceiling of about **76 frames per second**.

It is the **bottleneck of the whole pipeline**, and it is on the bus, not
in the computation:

| Frame rate | Period | Share taken by the write | Left for the effect |
|---|---|---|---|
| 60 fps | 16.7 ms | **78%** | ~3.6 ms |
| 30 fps | 33.3 ms | **39%** | ~20 ms |

> ⚠️ **60 fps only held in appearance.** The write alone ate more than
> three quarters of the period, leaving the effect less than the computation budget
> granted to it — so an effect **well within its limits** already caused the
> deadline to be missed, and the loop silently dropped to a frame rate it
> announced nowhere. The engine's frame rate was brought back down to **30**.

Two known reservations, should the frame rate need to go back up:

- **all 6 rows are rewritten on every frame**, without looking at what changed,
  even though partial writes are verified on hardware (§4);
- **the 7th report frame is re-sent on every frame** even though the device is already in
  custom mode — ~1.9 ms of the 13 on its own.

### Real frame — row 0 entirely red

```
00 9f 00 00 00 47 0f 03 00 00 00 00 15
ff 00 00  ff 00 00  ff 00 00  ff 00 00  ff 00 00  ff 00 00
ff 00 00  ff 00 00  ff 00 00  ff 00 00  ff 00 00  ff 00 00
ff 00 00  ff 00 00  ff 00 00  ff 00 00  ff 00 00  ff 00 00
ff 00 00  ff 00 00  ff 00 00  ff 00 00
00 00 00 00 00 00 00 00 00
5e 00
```

`0x5e` = XOR of bytes 2 to 87. It is the value expected by the
`checksum_matches_captured_frame` test of `candeo-protocol`.

---

## 6. Matrix — 132 and 106 are not the same thing

**6 rows × 22 columns = 132 cells**, of which **106 carry a key LED**.

| Figure | Meaning |
|---|---|
| **132** | matrix cells, and what the zone declares. **Size of a frame.** |
| **106** | cells carrying a physical key |

> **The trap.** A frame must cover all **132** positions. Sending fewer leaves
> the last rows frozen at their previous value — symptom observed during
> the survey: the bottom row stayed white while the rest changed color.

> An earlier survey announced 107 occupied positions: a counting artifact, the
> sentinel value `0xFFFFFFFF` having been counted as a distinct index.

```
rangée 0 :   0  --   2   3   4   5   6   7   8   9  10  11  12  13  14  15  16  --  --  --  --  --   (16)
rangée 1 :  22  23  24  25  26  27  28  29  30  31  32  33  34  35  36  37  38  39  40  41  42  --   (21)
rangée 2 :  44  45  46  47  48  49  50  51  52  53  54  55  56  57  58  59  60  61  62  63  64  --   (21)
rangée 3 :  66  67  68  69  70  71  72  73  74  75  76  77  78  79  --  --  --  83  84  85  --  --   (17)
rangée 4 :  88  89  90  91  92  93  94  95  96  97  98  99  -- 101  -- 103  -- 105 106 107 108  --   (18)
rangée 5 : 110 111 112  --  --  -- 116  --  --  -- 120 121 122 123 124 125 126  -- 128 129  --  --   (13)
```

### Index → key mapping

Reconstructed by cross-referencing the matrix with the ordered list of names the device
declares. The per-row counts add up exactly: 16 + 21 + 21 + 17 + 18 + 13 = **106**.

| Row | Keys |
|---|---|
| 0 | Esc, F1→F12, PrtSc, ScrLk, Pause |
| 1 | number row, Backspace, Ins/Home/PgUp, NumLock, `/ * −` |
| 2 | Tab, top row, Del/End/PgDn, numpad 7 8 9 + |
| 3 | CapsLock, home row, Enter, numpad 4 5 6 |
| 4 | Left Shift, ISO key, bottom row, Right Shift, ↑, numpad 1 2 3, numpad Enter |
| 5 | Ctrl/Win/Alt, Space, AltGr/Fn/Menu/Ctrl, ← ↓ →, numpad 0 . |

> **The ISO Enter carries two LEDs**: index **57** (row 2) and **79** (row 3). A
> vertical gradient is visible there — that is the hardware, not a rendering defect.
>
> **The space bar carries only one**: index **116**, at `(5, 6)`, despite being
> 6.25 units wide.

### Physical geometry

**The device does not declare it.** Only the 6 × 22 logical grid is available.
The realistic drawing used by the interface is written by hand from the standard
full-size ISO layout — see `docs/design/studio.md`.

The index-by-index expansion of the table above, and the rectangle of each key,
live in `crates/candeo-device/src/layout.rs`. **The two do not have the same status**:
the names are a survey, the geometry a convention. Only the former can be verified
against the device.

---

## 7. Exposed modes

| SDK index | Name | Protocol effect | Animated by |
|---|---|---|---|
| 0 | `Direct` | `0x08` | **the host** |
| 1 | `Off` | `0x00` | — |
| 2 | `Static` | `0x01` + RGB | firmware |
| 3 | `Breathing` | `0x02` + RGB | firmware |
| 4 | `Spectrum Cycle` | `0x03` | firmware |
| 5 | `Wave` | `0x04` + direction + speed | firmware |

So the six SDK modes correspond exactly to the six identifiers that
the device accepts — no more (`0x05` and `0x07` are refused), no fewer. The
match amounts to a cross-confirmation of the two surveys.

Firmware effects **survive the host software shutting down** and cost no
CPU time. `Direct` mode requires a continuous push of frames — that is the
cost of a software effect engine, and the reason why a user effect
must be able to run without a UI.

---

## 8. Reading the device — `GET_REPORT` and class `0x00`

**The device responds.** This is what was missing to tell an *accepted* write
from an *understood* write — the only thing that today separates a
keyboard that obeys from a keyboard that silently discards our frames.

### Read transport

`HidD_GetFeature` on Windows, `hid_get_feature_report` via hidapi. **Same
MI_03 interface, same 91-byte buffer** as writing. The command is written,
then read back: the response reuses the structure from §3, with two useful
differences — byte 0 carries a **status**, and bytes 6 and 7 **echo back**
the class and the command, which makes it possible to check that we are indeed reading the response
we expect and not the previous one.

### The status byte (offset 0)

| Value | Meaning |
|---|---|
| `0x00` | none |
| `0x01` | busy |
| `0x02` | **understood** |
| `0x03` | failure |
| `0x04` | timed out |
| `0x05` | **not supported** |

**Verified that it actually discriminates**, rather than assumed: a nonexistent
class (`0xee`) and a nonexistent command on a valid class
(`0x0f`/`0xee`) both return `0x05`, whereas a valid command returns
`0x02`.

**The response checksum is correct** (byte 88, the XOR of bytes 2 to 87, as
in a report): every response to an open's inspection carried one — firmware,
serial, brightness and effect reads, their rewrites and read-backs, 8 of 8 —
firmware v1.5, 14/09/2026. The inspection therefore rejects a response whose
checksum does not match, as a garbled reply (#74).

⚠️ **Measured limit**: an aberrant argument size on a valid command
still returns `0x02`. The device validates the **class/command pair**, not the
consistency of its arguments. A compatibility check can therefore only assert
"this command exists", never "my arguments are right".

### Class `0x00` — information

Surveyed on 12/09/2026 on our unit, firmware v1.5.

| Command | Response | Meaning | Established by |
|---|---|---|---|
| `0x81` | `01 05` | **firmware version — v1.5** (major `01`, minor `05`) | match with the version declared elsewhere |
| `0x82` | *(masked)* | **serial number** (15 ASCII chars) | format, and stability across reads |
| `0x83` | `01 25` | **unknown** | unknown to OpenRazer as well |
| `0x84` | `00 00` | **device mode** — `0x00` normal, `0x03` driver | see below |
| `0x85` | `01 00` | **polling rate** — `01`=1000 Hz, `02`=500, `08`=125 | |
| `0x86` | `04 80` | **locale layout** — `04` = `fr_FR` | verified: our `layout.rs` is indeed AZERTY |
| `0x87` | `01 05` | **unknown** | unknown to OpenRazer, "variable return values" |
| `0x80`, `0x88`–`0x8f` | — | `0x05` not supported | |

The value returned by `0x82` is exactly the one in §1 — it is **the** source of the
serial number, and the only one.

⚠️ **The USB descriptor carries no serial number** (`serial_number()` is
empty on all four interfaces) — only this command provides one. And
`release_number` is `0x0200` across the whole composite while the firmware
is at v1.5: **the `bcdDevice` is a hardware revision, not a firmware
version.** Do not confuse them.

### Class `0x0f` — reading the lighting back

This is the most useful part of the return direction: **it makes it possible to verify an effect
without relying on the eye**, which the first sweep of the
identifiers sorely lacked.

| Command | Observed response | Meaning |
|---|---|---|
| `0x82` | `00 00 <effet> <p1> <p2>` | **current effect**, parameters included |
| `0x84` | `00 00 ff` | **current brightness** |
| `0x80` | `05 19 03 06 16` | descriptor — `06 16` = **6 rows × 22 columns**, our matrix |
| `0x81` | `05 00 01 02 03 04 05 06 07 08 09` | enumeration of 10 values, **meaning not established** |
| `0x86` | `00 01` | not established |

⚠️ **`0x81` is NOT the list of supported effects**, even though it looks
like it: it contains `05` and `07`, which the device refuses in practice. This is
exactly the kind of coincidence that must be tested rather than concluded from.

**Effect verification method**: set the effect, wait ~150 ms, read it back
via `0x82`. If the identifier read back differs from the one set, the device **ignored**
the command — even if the write returned `0x02`.

### The device mode — and why Candeo leaves it alone

`0x00`/`0x84` returns `0x00`, i.e. **normal mode**, and our custom lighting
works perfectly that way. OpenRazer, for its part, switches devices to **driver
mode** (`0x03`, via `0x00`/`0x04`) when its daemon initializes.

The reason is documented on their side, and it explains why **we must not
imitate it**: in driver mode, the firmware **stops handling some
keys itself** and merely emits HID events that the host is
expected to pick up. If nobody is listening, those keys no longer do anything — a case
observed on a Basilisk V3, whose DPI cycle and scroll wheel lock
became inert, fixed by switching back to normal mode.

OpenRazer is a **full driver**: it handles macro keys, DPI,
profiles, so it needs the firmware to hand over control. **Candeo only
controls the lighting.** Switching to driver mode would bring us nothing and
would break keys that the device handles very well on its own.

> **Decision: never write `0x00`/`0x04`.** To be carried as an explicit
> warning in the device SDK (#34) — this is typically the step a
> contributor would copy from an existing driver without seeing what it costs.

---

## 9. Reproducing the survey

```powershell
# 1. Repérer le hub portant le clavier
tshark -D
tshark -i \\.\USBPcap1 -a duration:6 -w test.pcap
tshark -r test.pcap -Y 'usb.idVendor == 0x1532' -T fields -e usb.device_address

# 2. Capturer en poussant des couleurs pures espacées dans le temps
tshark -i \\.\USBPcap1 -a duration:30 -w capture.pcap

# 3. Isoler les transferts de contrôle sortants
tshark -r capture.pcap -Y 'usb.device_address == 9 && usb.transfer_type == 0x02 && usb.endpoint_address == 0x00' -T fields -e frame.number

# 4. Extraire les 90 octets
tshark -r capture.pcap -Y 'frame.number == 113' -V | Select-String 'Data Fragment:'
```

**The methodological point that unlocks everything**: send **pure, uniform colors**,
well separated in time. `ff 00 00` repeated 22 times jumps out in a
hex dump, and the byte positions give the component order without inferring it.

### Tooling pitfalls

- USBPcap only attaches its filter to hubs **after a reboot**.
- `USBPcapCMD --extcap-interfaces` may return nothing, even elevated. Go
  directly through `tshark -i \\.\USBPcapN`, which works.
- USB bus numbers change from one boot to the next.

---

## 10. Still to establish

- [x] **Content of the device's responses (`GET_REPORT`)** — §8
- [x] **Read the firmware version** — `0x00`/`0x81`, §8
- [x] **Obtain a serial number** — `0x00`/`0x82`, the USB descriptor carries none
- [x] **Exact commands for `Static` and `Breathing`** — `0x01` and `0x02`, size `0x09`, `args[5]=01` then RGB, §4
- [x] **Read back the current effect** — `0x0f`/`0x82`, which makes it possible to verify without the eye
- [x] **Which effect identifiers the device accepts** — the six from the SDK; `0x05` and `0x07` are refused
- [ ] Meaning of arguments 0 and 1 (offsets 8 and 9), constant at `0x00` — a third-party driver names them *variable storage* and *LED identifier*, unverified
- [ ] Is the transaction identifier (`0x9f`) checked by the device?
- [ ] Actual range of the `Wave` speed; the direction is bounded to `00`–`02`
- [ ] Effect identifier `0x06`: never tried
- [x] **Maximum throughput accepted before the device drops out** — see below
- [ ] Does the device accept a report shorter than 90 bytes?
- [ ] Meaning of `0x0f`/`0x81` (enumeration `00`…`09`) and `0x0f`/`0x86` (`00 01`)
- [ ] The first three bytes of `0x0f`/`0x80` (`05 19 03`), whose next two do give 6×22
- [ ] Meaning of `0x00`/`0x83` (`01 25`) and `0x00`/`0x87` (`01 05`) — unknown to OpenRazer too
- [ ] Second byte of the locale layout, `0x86` → `04 80`: what does `0x80` mean?
- [x] **A phantom HID entry `interface -1`** — virtual collection of the manufacturer's driver (`RZVIRTUAL`), not the keyboard; ruled out by the interface filter, §1
- [ ] Does reading back `Statique` and `Respiration` via `0x0f`/`0x82` return the color, and at which position? Until established, the inspection on open does not rewrite them
- [ ] **Is rewriting the current effect and brightness identically invisible?** That is the assumption that allows the inspection to send on every open — to be confirmed by `probe_inspection_on_open`, with the application closed

---

## 11. Log

| Date | Event |
|---|---|
| 2026-09-11 | Hardware identification, survey of the 6 modes via a third-party SDK |
| 2026-09-11 | Installation of Wireshark 4.6.8 and USBPcap 1.5.4.0 |
| 2026-09-12 | **First capture.** Transport, structure, RGB order, row indexing, checksum |
| 2026-09-12 | 6×22 = 132 matrix confirmed; fix for a 106-position send that left row 5 frozen |
| 2026-09-12 | **Full command set**: `0x02` effect, `0x03` row, `0x04` brightness |
| 2026-09-12 | **Validation by direct write** via `HidD_SetFeature`, with no third-party software. 91-byte buffer and partial row write confirmed |
| 2026-09-12 | Clarification of 132 / 106 and index → key mapping |
| 2026-09-12 | **The device responds.** `GET_REPORT` surveyed: status byte, class/command echo, and verification that a `0x05` does distinguish an unknown command from a valid one |
| 2026-09-12 | **Class `0x00` surveyed**: firmware (`0x81`), serial number (`0x82`), mode (`0x84`), polling rate (`0x85`), locale layout (`0x86`). `0x83` and `0x87` remain unknown |
| 2026-09-12 | Established that `release_number` (`bcdDevice`, `0x0200`) **is not** the firmware version (v1.5), and that the USB descriptor carries no serial number |
| 2026-09-12 | **Decision: never switch to driver mode.** It would make the firmware stop handling some keys, with nothing in return for a lighting controller |
| 2026-09-12 | **`0x0f`/`0x82` reads back the current effect** — verifying an effect without relying on the eye. `Static` (`0x01`) and `Breathing` (`0x02`) finally established: they require a color, and the first sweep set them in black |
| 2026-09-12 | **Identifiers `0x05` and `0x07` are refused** by this device, even though the write returns `0x02`. Live demonstration that a status byte does not validate the arguments |
| 2026-09-12 | **Throughput measured**: 13.1 ms per full update, ceiling ~76 fps, no refused write. The bottleneck is the bus, not the computation — the engine frame rate goes from 60 to **30 fps** |
| 2026-09-13 | **The `interface -1` entry explained** by the device tree: virtual HID collection under `RZVIRTUAL`, `RzDev_0292` service of the manufacturer's driver — not the keyboard |
| 2026-09-14 | **Response checksums are correct**: 8 of 8 responses to an open's inspection (§8) |
| 2026-09-14 | **Rewriting a running Wave identically shows no visible restart**, watched on the keyboard while the device was reopened (Ignore, then Control); the effect read back identical |

## 12. Captures

| File | Content |
|---|---|
| `deathstalker-*.pcap` | reference: red / green / blue / black / white |
| `fix132-*.pcap` | validation of the full 132-position send |
| `modes2-*.pcap` | effect switches: Spectrum, Wave, Off, Direct |
