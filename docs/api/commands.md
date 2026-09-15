# Tauri commands — the surface exposed to the front end

Defined in [`apps/desktop/src-tauri/src/lib.rs`](../../apps/desktop/src-tauri/src/lib.rs).

The serialized types live in the Tauri layer, **not** in the crates: this keeps
`candeo-protocol` and `candeo-device` free of any dependency on serde or
Tauri, and therefore reusable outside the application and testable in CI.

The state is a **table of open devices**, keyed the way adoption identifies
them. The decision, the open handle, the render loop, the running effect
and the error state are all held **per device**: nothing is implicit any more,
nothing is unique any more.

---

## Designating a device

Every command that acts on a device takes one, as an object with two
fields:

```ts
type DeviceRef = { vid: number, pid: number }
```

**VID and PID, nothing else.** That is what identifies a device at adoption, and
it is the key of all three tables: open devices, open failures, render
loops. The serial number tells apart two units of the same model in
`settings.json`, but it cannot serve as a key here — a silent enumeration
(hidraw without a udev rule) declares none, and the device would become
impossible to designate.

An object rather than two integers side by side: the same shape goes out as an
argument and comes back in the state the engine returns, and swapping two `number`
would only show at run time.

> The adoption commands — `adopt_device`, `ignore_device`, `connect` — keep
> `vid` and `pid` separate: they designate a **catalog layout**, not an
> open device, and one of them is precisely what brings that device into existence.

### Lock acquisition order

A deadlock has already been caught on this code base: `list_devices` took the
keyboard lock then the failures lock, `ignore_device` the reverse. With a table
of devices and N render loops, the rule is explicit:

> **No code holds two locks at the same time.** A table is locked for as long as
> it takes to read or store a shared pointer in it — never for an HID
> write, a loop start or a wait for completion.

Where two would become unavoidable, the order is the declaration order in
`AppState`: device table → engine → failures table → a device's
handle → a loop's shared state. The render thread only knows the
last two: it has no way to take an application lock, and therefore
no way to block a command with one. The only wait performed while holding a lock is
the one in `stop` — and that lock is **specific to the device**, which is exactly
what prevents stopping one device from holding up the commands aimed at the others.

---

## Discovery and adoption

### `list_devices() -> DeviceInfo[]`

Lists **all known layouts**, plugged in or not, and in which state.

```ts
{
  name: string,
  vid: number, pid: number,
  present: boolean,
  state: 'detected' | 'adopted' | 'ignored',
  open: boolean,
  error: Failure | null,        // see Errors
  surveyedFirmware: string,     // 'v1.5' — donnée du gabarit, connue sans rien ouvrir
  firmware: string | null,      // lu à l'ouverture ; null si fermé ou non lu
  warnings: string[]            // in the interface language; empty = nothing to report, NOT "compatible"
}
```

`present` is true if the VID, the PID **and** the interface number match
an enumerated device. The UI must show an absent layout as absent,
not omit it — that is what makes it possible to say "plug in your keyboard" rather
than show an empty list.

The interface number is required to equal the layout's, which rules out the
`interface -1` entry that the Windows enumeration carries on the same VID and PID:
an HID collection of the manufacturer driver's virtual node, not the keyboard (§1 of the
survey).

### What the device says about itself, on open

Every open — adoption at startup, adoption on demand, `connect` —
**inspects** the device, once. `Keyboard::open` does it, and nowhere
else: an open path that forgot it cannot exist.
Details in [`crates/candeo-device/src/inspection.rs`](../../crates/candeo-device/src/inspection.rs).

| Question | Command | What we do with it |
|---|---|---|
| which firmware? | `0x00`/`0x81` | `firmware`, compared with `surveyedFirmware`: **warns, does not block** |
| which unit? | `0x00`/`0x82` | adoption matching — the USB descriptor carries none |
| is brightness known? | read back, rewritten **unchanged**, read back | refused if the device returns `0x05` |
| is the effect known? | same, only if the current effect can be rewritten unchanged | same |
| the row? | **never sent** — no row write is invisible | unverified |

**Warn without blocking.** A version different from the surveyed one produces a
warning in `warnings`, shown on the device's line and logged at
`warn` — never a refusal. Blocking would make the application useless after a
routine update, when the protocol will very probably not have changed.

**A command the device declares unknown (`0x05`) is no longer sent**:
the write fails with a message that says so, and that failure propagates through
`deviceError` like any other write refusal. Without this, `hidapi` would accept
every report, the loop would consider itself healthy, and `reachingKeyboard` would stay green
above a keyboard that discards everything.

⚠️ **What this check does not tell.** The device validates the class /
command pair, **never the value of an argument**: setting effect `0x05`, which this
keyboard refuses, still returns `0x02`. A "known" command is known, and
that is all. Reading back after rewriting detects an argument understood
**differently** — the device would set something other than what was rewritten — but not
an ignored command, which leaves the value unchanged.

None of this is done again afterwards: reading back costs a USB round trip, and the
render loop has no time for it.

`present` says what the system sees, `state` what the user decided:
**the two are independent**. A controlled device can be unplugged, a
plugged-in device can be ignored. Merging them into a single field would make
"controlled but unplugged" impossible to express.

`error` carries the last open failure **of this device**. A table keyed
by VID/PID, not a global message: that is what keeps a failing device
from dragging any other down with it. A single field would force choosing which one to show,
and the next would erase the previous one.

### The three states, decided once and remembered

| `state` | At launch |
|---|---|
| `adopted` | opened automatically, without asking anything |
| `detected` | listed, but **not** opened |
| `ignored` | left alone, and it stays that way |

### Plugged in, unplugged

Candeo listens to the system instead of enumerating on a timer (#81, `src-tauri/src/hotplug.rs`): HID interface notifications on Windows (`CM_Register_Notification`), kernel uevents for `hidraw` nodes on Linux. A notification only says "look again", and nothing depends on how fast the machine is: notifications arriving during a pass cause another one, a burst is gathered into one pass (150 ms of quiet) only to save work, and an adopted device that is plugged in but does not open yet — firmware starting, permissions not applied — is tried again after 0.25, 0.5, 1, 2 and 4 s. Each pass compares what is plugged in with what is open:

- a device that left is **closed**, and its open failure forgotten. A loop running on it keeps running, writing nowhere;
- an `adopted` device that came back is **opened** as at startup — serial checked, brightness reapplied — and its applied effect resumes (`set_resume_effects`). A loop still running finds the handle filled again and carries on;
- the window and the tray are told through `candeo://etat-change`.

Without notifications — refused by the system — nothing changes: a replugged device is reconnected from **Devices**.

**The default is `detected`.** A device never seen before is listed, not controlled:
writing to a USB device one understands poorly is not harmless, and at the
scale of a growing catalog — keyboards, mice, memory, fans —
adopting by default is the way to break someone's hardware. There are also
the devices one does *not want* controlled: a vendor driver already in
place, or an uncertain survey.

### `adopt_device(vid, pid) -> LayoutInfo | null`

Remembers `adopted` for this device, and opens it if it is plugged in.

The decision is written **whatever the outcome of the open**: it is a
decision, not the report of an attempt. The next startup will replay it —
which is precisely what we want for a keyboard that a hub has not finished
enumerating.

The open happens **before** the write, for a single reason: it is the open that
reads the serial through the protocol. Written first, the decision would be remembered
without a serial, and therefore for any unit of the model. If the write fails, the handle is released
— nothing is open without a decision to justify it.

Returns the layout when the device was opened, `null` when it is adopted but
unplugged: that is not an error, it will be opened the next time it is plugged in. An
open that fails, on the other hand, propagates its message — and leaves it in `error`.

The other open devices stay open: adopting this one is not a choice made on
their behalf.

### `ignore_device(vid, pid)`

Remembers `ignored`, and **closes** the device if it was open: one does not keep
open what one commits to no longer touching. The handle is emptied, not removed —
the loop that fed it holds a copy, notices at the next frame
and stops writing, without the other devices being affected.

Does not go through HID, deliberately — ignoring a device must remain possible
precisely when HID access is the problem.

> There is no command to bring **one** device back to `detected`. The two
> decisions that matter are "control it" and "leave it alone"; a
> third button to return to indecision answers no question one
> asks in front of the screen. `reset_settings` (§Settings) brings them **all** back at
> once, and that is a different question: starting again from a known state.

### `connect(vid, pid) -> LayoutInfo`

**One-off** open, without deciding anything: does not touch `settings.json`,
and therefore does not survive a restart. This is what we want to try a device
without committing; `adopt_device` is what we want so as never to have to do it again.

Fails if no known layout matches, or if the HID open fails.

### `disconnect(device)`

Explicit release of **one** device. The others are not affected.

With `connect`, the pair that opens and closes **without deciding**, where
`adopt_device` and `ignore_device` write to `settings.json`. Both are
wrapped by `useDevice` and no screen calls them yet: what is missing
is a button, not a command.

> **`is_connected` was removed** (audit #65). It answered what
> `list_devices` already carries in the `open` field of each device, and what the
> window reads from there — a second source of truth for a state that only the Rust side
> knows. Querying device by device what a single command
> enumerates brought nothing, and made the two answers diverge the day
> one of them was refreshed without the other.

### At startup

The application itself opens **all** `adopted` devices that are present, before
showing the window. Each attempt is isolated: an open that fails
does not interrupt the loop, leaves its message on its device, and the following ones
open normally.

The serial read on open is checked against the decision. If it designates a different
unit from the one that was adopted, the handle is released and the device's
line says why — through the serial's fingerprint, never the serial. The
"Piloter" (Control) button then remains offered: it adopts the plugged-in unit in turn. An
earlier adoption, remembered without a serial, **learns it** at the first startup that
reads it, and `settings.json` is rewritten only in that case.

Unreadable settings or unavailable HID do not prevent startup — that
would remove the only means of fixing the situation. Nothing is opened, the
reason goes to the log (`tracing::error!`, hence into the day's file), the
window is shown.

---

## Layout

### `get_layout(device) -> LayoutInfo`

Fails if this device is not open.

```ts
{
  name: string,
  rows: number,          // 6
  cols: number,          // 22
  frameLen: number,      // 132 — taille d'une image
  keys: {
    index: number,       // rang dans une image
    row: number, col: number,   // position dans la matrice logique
    scancode?: number,   // 0x01, 0x2A, 0x4E… — absent pour Fn
    label?: string,      // « ECHAP », « MAJ », « + (PAVE NUM.) »… — nommé par le système
    x: number, y: number, w: number, h: number   // rectangle physique
  }[]                    // 106 entrées
}
```

> The DTOs carry `#[serde(rename_all = "camelCase")]`: the Rust field `frame_len`
> therefore arrives as `frameLen`. Rust naming must not leak through into
> the UI — that is an abstraction leak that would only show at run time.

> **`frameLen` and `keys.length` differ, and that is intended.**
> `frameLen` is **132** — every cell of the matrix, gaps included: that is
> what a frame must cover. `keys` holds only **106** of them, the ones carrying
> a physical LED: that is what the simulator draws and what an effect iterates over.
>
> Confusing the two is the trap of this hardware. See
> [`../protocol/deathstalker-v2-pro.md`](../protocol/deathstalker-v2-pro.md) §6.

### Scancodes and labels

`scancode` is what the keyboard sends for the key, in PS/2 set 1 as Windows
reports it: the make code, with `0xE0` in the high byte for an extended key
(`0xE01D`, right Ctrl) and `0xE11D` for Pause. It names the physical key
whatever its legend — `0x11` is engraved Z here and W on a QWERTY keyboard — so a
key is found by its scancode, and a key press finds its LED the same way. The two
arms of the ISO Enter are both `0x1C`; every other scancode names one key. Fn
sends nothing and has none. Like the geometry, scancodes follow from the position
and are written in the layout, not read from the device.

`label` is **not written anywhere**: the application asks the system for the
key's name in the keyboard layout it uses (`GetKeyNameTextW` on Windows), so a
QWERTY system says "W" where a French one says "Z". That call reads the layout
without touching its state; `ToUnicodeEx` would consume a pending dead key and
break accents typed in other applications. `GetKeyNameTextW` swaps Pause and Num
Lock against what the keyboard sends, and `keys.rs` swaps them back. There is no
label on other systems yet.

The simulator draws no label: none fits a keycap at preview size, and only the
arrangement matters to an effect.

### The geometry is not read from the device

`x` / `y` / `w` / `h` are in **keyboard pitch units** — 1 u = the width
of a letter key — origin at the top left, `y` pointing down. The complete
drawing measures 22.5 u × 6.5 u.

These rectangles **do not come from the hardware**: it only exposes the 6 × 22
grid and declares no dimension. They are a hand transcription of the
full-size ISO key arrangement, written in
[`crates/candeo-device/src/layout.rs`](../../crates/candeo-device/src/layout.rs).
A drawing mistake breaks no consistency test: it can only be seen by
eye, on the simulator.

Two hardware quirks surface here:

- **The ISO Enter key carries two LEDs** (indices 57 and 79) and therefore appears in `keys`
  **twice**, under the same `name`. The two rectangles are the two adjoining
  arms of the L — they do not overlap, and a renderer that paints them separately
  reproduces the vertical gradient visible on the device.
- **The space bar carries only one** (index 116), for a width of 6.25 u.

A renderer that assumes "one key = one LED" is therefore wrong in both directions.

---

## Lighting

The four commands write to **one** device, which they take as their first
argument, and fail if it is not open.

### `set_brightness(device, level: number)`

`level` from 0 to 255. Writes **to the keyboard**, and nothing else: it is a
separate protocol command (`0x0f`/`0x04`), unrelated to the running effect.

Remembering this level from one launch to the next is the job of `remember_brightness`
(§Settings), which only writes to disk. Same split as `set_effect_params` /
`remember_effect_params`, and for the same reason: dragging a slider produces
dozens of HID writes and a single disk write, when it stops.

### `set_effect(device, effect: EffectDto)`

Serde tagging on the `kind` field:

```ts
{ kind: 'off' }
{ kind: 'spectrumCycle' }
{ kind: 'wave', direction: number, speed: number }
{ kind: 'custom' }
```

The first three are executed **by the firmware**: zero CPU cost,
and they survive the application closing. `custom` switches the keyboard to
host-controlled mode, which requires a continuous push of frames.

### `present(device, frame: number[])`

Complete frame: a flat sequence of RGB triplets, **exactly `frameLen × 3` bytes**
(396 for the DeathStalker). A different size is refused with an explicit
message rather than writing partially.

Internally: six `0x0f`/`0x03` transfers, one per row, then a switch to the
`custom` effect.

### `write_row(device, row, col_start, colors: number[])`

Writes a row segment without touching the rest — partial writes are supported
by the device, verified on hardware. Useful for localized effects,
which thus avoid resending all 132 positions.

---

## Effect library

The model is fixed in
[`../design/effects-library.md`](../design/effects-library.md) and
[`../design/effects-sources.md`](../design/effects-sources.md). No path is
hard-coded: Tauri's API resolves every folder.

```
app_data_dir()/effects/<name>.ts                 the shipped effects
document_dir()/candeo/effects/<name>.ts          the user's effects
app_cache_dir()/effects/<source>/<name>.json     JavaScript, manifest and swatch compiled from one version of a file
app_config_dir()/settings.json
```

**An effect is a file, and its key is its source and its name**:
`shipped:Breathing`, `user:My effect`. Adding an effect is saving a `.ts` file in
the user's folder. The key is what everything that refers to an effect holds —
`settings.json`, the engine, the tray menu, editor drafts — and the name, its
file name, is what the interface shows. A shipped effect and a user effect may
share a name. Where the system names no documents folder, the user's folder is
`app_data_dir()/user-effects/`.

Names follow the Windows rules on every system: no `< > : " / \ | ? *` or control
characters, no leading or trailing space or dot, not a reserved device name
(`CON`, `NUL`, `COM1`…), at most 64 characters. Two names that differ only by case
are one effect.

**Rust cannot strip TypeScript types, the window can.** Listing reports each
file with the hash of its bytes and whether the cache holds a result for that
hash; the window compiles what is `stale` — at startup and on Refresh — and hands
the JavaScript back through `cache_effect`. The engine and the tray only run
JavaScript compiled from a file's **current** bytes.

### `list_effects() -> EffectEntry[]`

```ts
{
  id: string,                     // the key, `shipped:<name>` or `user:<name>`
  kind: 'builtin' | 'user',       // `builtin`: a file of the shipped folder
  state: 'ready' | 'stale' | 'broken',
  error?: string,                 // pourquoi un effet `broken` ne se charge pas
  hash: string,                   // SHA-256 du fichier
  modified: boolean,              // un intégré modifié hors de l'application
  swatch: string[],               // couleurs « #rrggbb », prélevées sur le rendu
  name: string,
  description: string | Record<string, string>,   // une chaîne, ou une par langue
  params: Record<string, ParamSpec>,
  apiVersion: number,
  readsKeys: boolean              // déclare `inputs: ['keys']`
}
```

The shipped effects, then the user's, each sorted by name regardless of case.
`kind` is `builtin` for a file of the shipped folder recorded as copied by the
application — modified or not — and `user` for a file of the user's folder. A file
of the shipped folder Candeo did not put there is not listed: the next startup
moves it to the user's folder. A built-in is listed in its own gallery section,
and is not deleted, renamed or saved over (§The shipped effects). `modified` is
true for a built-in whose file no longer has the recorded hash.

| `state` | Meaning |
|---|---|
| `ready` | compiled for the file's current bytes: it can be previewed, applied, offered in the tray |
| `stale` | never compiled, or changed since: only `name` and `hash` are meaningful until the window compiles it |
| `broken` | compiled for the current bytes, and the module does not load: `error` says why. Not retried until the file changes |

Listing reads and hashes files, and **runs nothing**. A file that cannot be read,
or whose name the application would refuse, is skipped rather than failing the
whole list. The swatch travels with the entry, not behind a second call.

### `save_effect_source(key, source, create) -> string`

Writes `<name>.ts` in the user's folder, for a `user:<name>` key, and returns the
SHA-256 of what was written. `create` names the gesture, so that neither can do
the other's job by accident: creating refuses a name one of the user's effects
already has, in any case; saving again refuses a name none has. A `shipped:` key
is refused: a built-in is not saved over. The file is written through a temporary
file and a rename, so that a Refresh never compiles half a file.

### `cache_effect(key, hash, js) -> EffectEntry`

Records the JavaScript the window compiled from the version of the file that has
`hash`, and returns the entry. Refused when the file no longer has that hash: it
changed while the window compiled, and recording would pair this code with
another version of the source. The entry is marked built-in as in the listing.

Rust loads the module **once**, under the time budget swatch sampling uses, and
reads what it declares: `description`, `params`, `apiVersion`. `description`,
each parameter's `label` and each `choice` option's label are a string or a map
of languages (`{ en, fr }`), kept
as is: the window shows its language, then English, then the first entry. A
description that is neither is dropped. The parameters are
stored **as is**: their shape is that of `ParamSpec` in `@candeo/effects-api`, and
the Rust side does not interpret them. `apiVersion` is 1 when the module declares
none; an effect written for a version this application does not know is
recorded `broken`, with a message that says so — rather than failing at the first
frame. A module that does not load at all is recorded `broken` with its error.

### The color swatch

Each entry carries a few colors that help find an effect without running it.
**They are obtained by running the effect**, never declared in the manifest nor
drawn by hand.

Two reasons, and the second weighs more than the first. The author has nothing to
provide: you write your effect, it has its swatch — no field, no advanced mode.
And above all, **the swatch cannot lie**. Declared, it would drift from the first
change to the code, and an effect that turned blue would keep its red thumbnail.

#### How it is sampled

Four frames are rendered by the engine, without touching the hardware, at four
instants: 0 s, 0.37 s, 1.13 s and 2.61 s. The effect runs with the **default
values its module declares** — the ones the gallery would launch it with, and not
an empty object, which would give black for any effect that falls back on
nothing. The layout is the **default** one, never the plugged-in keyboard's: a
swatch that depended on the hardware present would be comparable neither from one
effect to another, nor from one machine to another.

From each frame **one** color is taken: the average of a diagonal band of the
keyboard, the band moving forward from one frame to the next. Three choices, three
reasons:

| Choice | Why not otherwise |
|---|---|
| **irregularly spaced** instants | regularly spaced, they would lock onto the period of a cyclic effect and return the same color four times |
| a **diagonal** band | a horizontal gradient varies only by column, a vertical sweep only by row: slicing along either would make the other perfectly uniform |
| a **band**, not a key | an effect can leave almost the whole keyboard off — `balayage` is exactly that — and an isolated key would land on black by chance |

The result: a uniform effect returns its color four times, a gradient returns four
staggered colors, a mostly dark effect returns a dark swatch. A spatial effect and
a uniform effect cannot look alike.

> **This document does not list the swatches of the shipped effects.** It used to
> carry a table of hexadecimal values, which **nothing checked against the
> result**: the day "Onde radiale" (Radial wave) moved from grid distance to
> physical distance, its four colors became wrong without any test, any build or
> any review flagging it. A swatch is sampled by running the effect; the library
> displays them, and that is where to look at them.

#### When it is computed, and where it is stored

**Once per version of a file**, by `cache_effect`, in the cache record next to the
JavaScript — never when the list is displayed, which remains a disk read.

#### What can go wrong

This is user code: it can throw, fail to load, or loop forever. Sampling is
time-bounded, otherwise a `while (true)` would block `cache_effect`. An effect
that loads but throws while rendering is `ready` with an empty `swatch`, and the
interface shows a neutral dot. An effect that renders black everywhere is not a
failure: its swatch is black, and that is the truth about what it does.

`swatch` is a **list**, not a quadruple, and nothing in storage fixes its length.

### The shipped effects

Fourteen are shipped, written in **TypeScript against the same API** as the user's
effects, in [`packages/effects/`](../../packages/effects/), and embedded in the
binary. At startup each one is copied into the shipped folder **once**, and
`settings.json` records it under `shippedEffects`, by name: the hash of the copied
version.

| State at startup | Action |
|---|---|
| not recorded, no file of that name | copy it, record its hash |
| not recorded, a file of that name exists | the file is not ours: it moves to the user's folder, and the shipped effect is copied |
| recorded, file unchanged, shipped version changed | overwrite it, record the new hash |
| recorded, file modified | leave it |
| recorded, file missing (deleted or renamed) | leave it: never copied again |
| recorded, no longer shipped by this version | forget the record; the file moves to the user's folder, and its references from `shipped:` to `user:` |

A file moving to the user's folder takes a ` (2)` suffix on a name already taken
there.

**The application does not delete, rename or save over a built-in**: an edited
copy stops receiving updates without a word, and a renamed or deleted one never
comes back. `duplicate_effect` makes an editable copy. The folder stays the
user's; what was done there is repaired on request, never at startup — see
`missing_builtins` and `restore_builtin` below.

| Name | Former id | What sets it apart |
|---|---|---|
| Radial wave | `onde-radiale` | moving hue, in circles at the physical distance of the keys |
| Diagonal wave | `onde-matricielle` | moving hue, in diagonals from a corner of the matrix |
| Breathing | `respiration` | a single color, no variation in space |
| Sweep | `balayage` | one lit row, the rest off |
| Fixed gradient | `degrade-fixe` | two colors, motionless — its `render` ignores `time` |
| Color wheel | — | every hue by angle around the center, turning |
| Noise map | — | two colors drifting in patches, value noise through time |
| Rain | — | drops falling down each column at its own pace |
| Starry night | — | keys twinkling over a dark sky |
| Bubbles | — | rings growing from random points and fading |
| Lightning | — | flashes striking along the keyboard over a dark sky |
| Crossing beams | — | a vertical and a horizontal beam sweeping across each other |
| Swirl circles | — | two glowing circles orbiting the center |
| Ripples | — | a ring spreading from every key pressed; reads key presses |

Color wheel to Swirl circles were written for Candeo after effects of the OpenRGB
Effects Plugin, from what they show, not from its code. None of the shipped
effects keeps state between frames: what a key shows depends on the instant, and
for Ripples on the presses the engine gives with it (`docs/design/key-input.md`),
drawn from deterministic hashes where it looks random.

The former ids are those of the version that compiled them into the binary: the
migration below moves the settings that still use them.

### `missing_builtins() -> string[]`

The names of the shipped effects with no file in the shipped folder, regardless of
case. A user effect of the same name does not hide one.

### `restore_builtin(name)`

Records the shipped effect's hash, then writes its file: a missing one comes back,
a modified one is overwritten, and both receive updates again. Loops running a
modified version keep the code they loaded until the effect is applied again.
Refused for a name this version does not ship. The user's effects live in another
folder, so restoring never touches them.

### `rename_effect(from, to) -> string`

Renames one of the user's effects, `from` being its key and `to` its new name, and
returns its new key. Renames the file and its cache, then moves every reference:
the settings on all devices, and the loops running the effect, preview included.
A running effect **keeps running**: the loops keep the code they loaded, only the
id they report changes. Refused for a built-in, and when `to` is not a valid name
or is already the name of one of the user's effects — except the same effect under
another case.

Renaming the file outside the application makes a new effect: the old key's
settings stay in `settings.json`, unused, and come back if the file gets its name
back.

### `duplicate_effect(id) -> string`

Copies the file into the user's folder under the first free name among `<name>`,
`<name> (2)`, `(3)`…, and returns its key: a built-in's first copy keeps its name,
under `user:`. Numbered rather than suffixed with a word, so a file name does not
depend on the interface language. The cache is copied with the file — same bytes,
same hash — so the copy is `ready` at once. A copy of a built-in is a new effect,
the user's.

### `open_effects_dir()`

Opens the user's effects folder in the system file manager, creating it on a first
launch. Adding an effect is saving a `.ts` file there.

### `forget_effect_settings(id)`

Forgets what `settings.json` keeps about an effect the folder no longer holds —
its parameters on every device, and where it was applied — without touching any
file. The gallery offers it next to the notice saying an effect it remembers is
missing; nothing is forgotten until then, so putting the file back under its name
restores everything.

### `delete_effect(id)`

Deletes the file and its cache. A name that is not valid is refused before any
disk access, and so is a built-in, before any loop is stopped.

Also takes away everything `settings.json` remembered about it: the **settings**, on
all devices, and its **application** (`activeEffects`). Forgetting comes after
deletion: if deletion fails, the effect is still there and its settings must be
too.

Purging `activeEffects` settles the trap raised by issue #48: without it, deleting
the applied effect would leave a **dangling identifier**. The invariant "the file
never contains an identifier the library does not know" can be checked without
running anything; the startup fallback remains necessary as a **second** barrier —
a file removed by hand does not go through here.

**Three steps, and the order is part of the contract:**

1. **the refusal**, first of all — a name that designates nothing gets a no
   without anything having been stopped or erased;
2. **stopping the loops** running this effect, on **all** devices and in the
   preview, before erasing. The engine keeps the JavaScript it loaded at start: a
   loop left alive would carry on without the slightest visible error, on a file
   that no longer exists;
3. **erasing**, then forgetting the settings.

Stopping is done **on the Rust side**, not in the window: it is the only place that
guarantees it whoever the caller is.

### `read_effect_source(id) -> string`

The source, to open it in the editor.

### `legacy_effect_ids() -> Record<string, string>`

What effects were called before **this run's** migration — directory ids, names —
and the keys they became; empty on every later run. Only for the editor's drafts,
stored in the web view's storage, which the migration cannot reach.

### Migration from the directory layout

At startup, before anything reads the library or the settings, each
`effects/<id>/` holding a `source.ts` becomes `effects/<name>.ts`, `<name>` being
its manifest's name made valid (forbidden characters replaced by `-`, ` (2)` on a
collision). `activeEffects` and `effectParams` are rewritten from the old ids to
the names, and `settings.json` records `version: 1` so that this happens once.
Then the former ids of the shipped effects move to their names, recorded as
`version: 2`, and the shipped effects are copied. Last, every file of that folder
Candeo did not copy moves to the user's folder, and every reference becomes a key
— `shipped:` for a recorded shipped effect, `user:` otherwise — recorded as
`version: 3`; the flat cache of version 2 is deleted and compiled again. Settings
are rewritten **before** files are moved, and a directory or file is removed only
once its copy is written: an interrupted run is finished by the next startup
without duplicating an effect.

---

## Settings

### `get_settings() -> Settings` · `set_settings(settings)`

```ts
{
  preferences: {
    logLevel?: 'error' | 'warn' | 'info' | 'debug' | 'trace',
    language?: 'en' | 'fr',       // absent : la langue du système
    resumeEffects?: false,        // absent: a device that opens resumes its applied effect
    logFilesKept?: number,        // absent: 7; 0 keeps every log file
    theme?: 'light' | 'dark'      // absent: the system's
  },
  devices: {
    vid: number,
    pid: number,
    serial?: string,             // absent quand le système n'en déclare pas
    state: 'detected' | 'adopted' | 'ignored',
    brightness?: number          // absent = pleine (255)
  }[],
  activeEffects: {
    vid: number,
    pid: number,
    effect: string               // l'effet appliqué sur cet appareil
  }[],
  effectParams: {
    vid: number,
    pid: number,
    effect: string,              // identifiant de l'effet réglé
    values: Record<string, ParamValue>
  }[]
}
```

**A global preference goes in `preferences`, anything that depends on a keyboard
in a keyed list.** That is the rule this file follows, and it holds for everything
added to it later: the language will go in `preferences`, with nothing to arbitrate.

Each list carries only what **differs from the default**: a device absent from
`devices` is `detected` and at full brightness, a device absent
from `activeEffects` has had no effect applied, and a device entry that
no longer remembers anything — `detected` with no brightness — is removed rather
than kept empty.

On first launch there is no file: `get_settings` returns the
**defaults**, it is not an error. A field missing from a file written by an
earlier version also takes its default, rather than leaving the
application silent at startup.

#### What disappeared in v2.1, and why

`activeEffect`, `device` and `brightness` were three scalars at the root. The
first two were neither read nor written by anyone; the third was. The
problem was not their value, it was their **shape**: they described *one*
active effect, *one* chosen device and *one* brightness level, whereas the engine
has been running one effect per device since issue #26 — and
`set_brightness(device, level)` already took a `DeviceRef`.

- `activeEffect` → `activeEffects[]`, one entry per device;
- `device` → **removed**. `devices` already carries the decisions device by
  device; a global "chosen device" has made no sense since there has been
  no implicit device;
- `brightness` → `devices[].brightness`. Two keyboards have no reason to
  share a level.

An earlier file is read back without error, and these three keys are simply
ignored: recovering them would have required choosing *which* device they
designated, a question with no answer. Only `logLevel`, which moved without
changing meaning, is **recovered** from the root and moved into `preferences` at
the first read — resetting it to the default would have silenced the very person
who was in the middle of hunting down a failure.

`activeEffects` is written by `start_effect` and cleared by `stop_effect`; the preview
never touches it. Deleting an effect purges its entry everywhere — see
`delete_effect`.

### `set_resume_effects(on)`

Whether a device that opens starts its applied effect again: at startup, on
adoption, and when it is plugged back in. On by default; only `false` is written. Rust resumes the effect itself,
with the settings saved for it on that device, so it works with the window
hidden. Nothing starts when the effect already runs there. An effect that cannot
start keeps its `activeEffects` entry and is logged; one edited outside Candeo
waits for the window to compile it, and `cache_effect` resumes it then.

### `get_launch_at_login() -> LaunchAtLogin` · `set_launch_at_login(on) -> LaunchAtLogin`

```ts
{
  enabled: boolean,
  available: boolean   // false in a development build, and on macOS
}
```

Launch at login, hidden in the notification area (#103). **The system's entry is
the only record**, not `settings.json`: the `Run` value under
`HKEY_CURRENT_USER` on Windows, `autostart/candeo.desktop` in the XDG configuration
folder on Linux, pointing at the AppImage itself when there is one. The system's
own tools change the same entry, so `enabled` reads it back: an entry Task Manager
turned off (`StartupApproved`) or a desktop marked `Hidden=true` is off. Turning
it on from Candeo clears Task Manager's "off".

The entry passes `--hidden`. The window is declared `create: false` and built at
the end of `setup`; launched with `--hidden`, it is built only when someone opens
it from the tray or launches Candeo again. Without a tray icon it opens anyway,
being the only way in.

A development build writes no entry: it would register a binary under `target/`.

### `reset_settings()`

Rewrites `settings.json` with the **default values**, and resets the devices.
It is the only way to return to a known state without editing the file by
hand — the first thing one looks for when something misbehaves, and what
makes a bug report usable.

What goes: the adoption decisions — everything goes back to `detected` —, the
remembered brightness of each device, the effect applied on each, and the
settings remembered per device / effect pair. `shippedEffects` stays: it describes
the folder, and without it every built-in would become the user's at the next
launch.

**No effect is touched.** Written effects live in
`app_data_dir()/effects/`, not in `settings.json`; removing them is a different
action, `delete_effect`, one per effect. That is the distinction the whole storage
layer holds to — an effect is content, the choice of the active effect is
configuration — and blurring it would lose hand-written code for someone who
only wanted to un-adopt a keyboard. The command does not have the
means anyway: it only writes to the configuration file.

The devices are reset **before** the write, in this order:

1. **the loops stop**, and the stop is awaited — resetting the device
   table while an effect is running would leave loops that no
   decision designates any more, and the next frame would turn back on what is
   about to be turned off;
2. **the backlight turns off** (`Effect::Off`). Stopping a loop leaves the
   keyboard on its last frame, and a frozen frame looks like an effect still
   running; turning off is executed by the firmware, it costs
   nothing;
3. **the handles are closed** and the open failures forgotten: one does not keep
   a device open that no decision designates any more, and a failure
   message describing an adoption that no longer exists teaches nothing.

A keyboard that refuses to turn off — unplugged in the meantime, access lost —
does not interrupt the reset: turning off is a nicety, not the point.

This is **not** a place to free resources on the effects side. Each loop
holds its own QuickJS `Runtime` and `Context`, both destroyed with it: the whole
JavaScript heap goes with them. No `dispose()` entry point is desirable,
it would put user code on the shutdown path.

The window, for its part, keeps what it had read: it is up to the window to forget the settings
held in memory after the call, otherwise the first slider movement would
write them back.

### `remember_effect_params(device, effect, params)`

Remembers an effect's settings **for one device**, and nothing else in the
file. Not to be confused with `set_effect_params`, further down, which adjusts the
running loop: this one writes to disk and changes nothing about what is running. The two have neither the same rate — dozens of calls per second
on one side, a single one when the slider stops on the other — nor the same destination.

A dedicated command rather than a `set_settings` from the window: reading,
modifying and writing happen on the Rust side, in one go.

### `remember_brightness(device, level)`

Remembers the brightness of **this** device, without touching the keyboard — the disk
counterpart of `set_brightness`. It is reapplied when the device is opened, at
startup as well as at adoption: a remembered level that was not reapplied when
plugged in would be useless, and the surveyed protocol can write brightness
but not read it back.

The maximum (255) **erases** the entry instead of writing the default into it, exactly
as an empty parameter table erases an effect's settings. A device
whose only decision it was then disappears from `devices`.

The serial is recorded if it is available, as for `ignore_device`: it makes
the level land on the right unit when there are two of the same model, and
its absence blocks nothing.

This is not a precaution against interleaving — synchronous commands
run on the main thread, they do not overlap. It is a
precaution against a **stale copy**: the window reads the settings once,
when the screen mounts, and a `set_settings` posted at the first slider movement
would send that snapshot back as is, erasing whatever had been decided since. This
is not a textbook case — `adopt_device` writes `settings.json`, and adopting a
device is precisely what one does between two adjustments.

An **empty** `params` table erases the entry: it means "restore the declared
values". The effect then starts again from its manifest, including if a later
version changes its defaults.

It is called **at the end of the gesture** — slider released, box checked — and not
after a debounce delay: closing the window destroys the web view without running its
exit hooks, and closing the window while an effect is running is the intended way
of using the application. A timer would therefore only ever be a safety net.

`delete_effect` takes away the settings remembered for the deleted effect, on all
devices. Otherwise the file would keep entries designating an identifier
that nothing names any more, and an effect reinstalled under the same name would silently
inherit the settings of its vanished namesake.

### The key of a setting: the device and the effect, without the serial number

The same effect has no reason to run at the same speed on two keyboards,
and two effects on the same keyboard do not have the same parameters: the key is therefore
the pair. Switching effects and coming back finds its settings again, and the screen
reads them back at the next startup.

**Without the serial, unlike `devices`.** All engine commands
target a `DeviceRef`, that is a VID and a PID; two units of the same
model already share their render loop. Telling them apart here would promise a
separation the rest of the application does not keep, and the setting would seem
lost one time out of two. Adoption, for its part, decides to open a specific unit:
it needs the serial, and that is why it carries it.

Like `devices`, `effectParams` contains only what **differs from the default**:
a parameter left at its declared value does not appear in it, and will follow the manifest
if the effect is saved again with other defaults. An entry therefore appears only
if someone has moved a slider.

### The identity of a device: VID / PID / serial number

**Neither the variant, nor the firmware.** The same keyboard reported itself as
`v1.4 / Unkown Variant` then `v1.5 / Quartz` during the protocol survey: a
binding that matches on these fields breaks at the update, and the adopted
device becomes a stranger overnight.

**The serial comes from the protocol** (`0x00`/`0x82`), read on open: the
DeathStalker's USB descriptor carries none. A closed device is not opened
just to ask it for the serial — an ignored device must be left alone — and it is
then the descriptor, silent, that serves as fallback.

The serial is only compared if **both sides** carry one, and this
trade-off holds in both directions:

- it tells apart two units of the same model — without it, adopting one
  would adopt the other;
- but a silent enumeration — hidraw without a udev rule, a hub that
  relays nothing — must not unmatch an already adopted device, otherwise the
  decision would have to be made again at every plug-in.

An entry learned without a serial is completed as soon as the serial is known; it is never
erased.

`devices` contains only the decisions that **differ from the default**: a device
absent from the list is `detected`, which is exactly the state of a device
never encountered. The file therefore does not grow by one entry for every
device plugged in once.

Writing goes through a temporary file followed by a rename: a power cut in
the middle of a write would otherwise leave truncated settings.

The name of this temporary file is **fixed**, and it can be because synchronous
commands run on the main thread: two read-modify-write sequences
do not interleave. The reasoning holds *within* a process — between
two, nothing would serialize them, and one would rename what the other is in the middle
of writing. It is therefore the **single instance**
([`single_instance.rs`](../../apps/desktop/src-tauri/src/single_instance.rs)) that
makes this fixed name safe: the two decisions go together, and cannot be undone one
without the other.

---

## Errors

All fallible commands return `Result<T, Failure>`, which reaches the window as:

```ts
{ code: string, params: Record<string, string> }   // { code: 'effectNotFound', params: { name: 'Rain' } }
```

`code` names `errors.<code>` in the catalogs, and the window says it in its
language (`message()` in `api/journal.ts`). Rust logs the English text. What
nobody can act on — a disk refusing a write — is `unexpected`, with an English
`detail`.

An effect's own load or render error (`EffectEntry.error`, the engine's
`error`) stays an English string, for its author.

---

## Log

`tracing` + `tracing-subscriber` + `tracing-appender`, initialized at the top of the
application's `setup` — **before the store is resolved**, otherwise a failure
to resolve the configuration folder would happen before there was anything to
write it with. Design and trade-offs in `src-tauri/src/journal.rs`.

The file rotates **daily**, in `app_log_dir()` — through the
Tauri API, never a hard-coded path: logs are neither data nor
configuration, and on Linux the three folders differ.

How many files are kept is a setting, `preferences.logFilesKept`: seven by
default, `0` for all of them, as OpenRGB's `file_count_limit`.
`set_log_files_kept(keep)` saves it and deletes the files beyond it at once;
otherwise Candeo deletes them at startup, once the settings are read, and when a
new day's file starts. Only `candeo.YYYY-MM-DD.log` files are ever deleted. The
cap counts files, not bytes: what bounds a day's size is the level.

| Level | What it means |
|---|---|
| `error` | the user's lighting is broken |
| `warn` | degraded but working — unexpected firmware (#35), inoperative instance exclusion (#45) |
| `info` | lifecycle: device adopted, effect started, effect stopped |
| `debug` / `trace` | per frame, **off by default** |

**Transitions, never occurrences.** At 30 frames per second, a failing write
would produce thirty lines per second and bury the only one that
matters. The log follows the engine's model exactly — "started failing
(reason)", "recovered", "stopped after 30 failures" — and nothing per frame.

**One span per render loop**, carrying the device and the effect. That is the reason
for choosing `tracing` rather than `tauri-plugin-log`: there is one loop per
device, and "écriture refusée" (write refused) is useless without knowing which one.

**The serial number appears nowhere.** A stable fingerprint (FNV-1a, 16
hexadecimal digits) replaces it: it tells apart two units of the same
model without disclosing which one. A silent enumeration — hidraw without a udev rule —
is reported as `aucune`, which is not the same thing.

### Level precedence

1. **`CANDEO_LOG` wins**, always — that is what makes it possible to diagnose
   an application that does not get far enough to read its settings. It accepts
   a bare level (`debug`) or a full `EnvFilter` directive
   (`candeo_desktop_lib::runtime=trace,warn`);
2. otherwise `settings.json`, field `logLevel`;
3. otherwise `info`.

`CANDEO_LOG` and not `RUST_LOG`: the latter is shared by all Rust
tooling, and someone who set it for `cargo` would unintentionally change the
application's log.

### `get_journal() -> JournalStatus`

```ts
{
  level: LogLevel | null,     // null : CANDEO_LOG porte une directive qu'aucun niveau ne résume
  setting: LogLevel | null,   // ce que retient settings.json ; null = le défaut
  forcedByEnv: boolean,
  dir: string | null,         // null : le journal n'écrit pas sur disque
  verbose: boolean            // le niveau actif porte du par-image
}
```

### `set_log_level(level) -> JournalStatus`

Changes the level **without restarting** (`tracing_subscriber::reload`), and
remembers it. The fault being chased may not survive a restart: a keyboard
that drops out after two hours, a device that disappears intermittently —
"restart in verbose mode" amounts to asking to reproduce what one has just
observed.

**It survives a restart**, and that is a trade-off: automatically returning to the
default would protect against a full disk, persistence serves whoever is tracking down a fault
at launch. The price is paid by `verbose`, which the UI displays.

When `CANDEO_LOG` is set, the setting is **written but not applied** — the
precedence holds for the whole run, not only at startup. It will apply at the
next launch without the variable, and `forcedByEnv` tells the UI to
announce it.

`reset_settings()` also brings the level back to the default, and immediately: it has
just been erased from the file, leaving it applied would make the screen lie.

## Interface language

### `get_language() -> LanguageStatus` · `set_language(setting) -> LanguageStatus`

```ts
{
  setting: 'system' | 'en' | 'fr',   // ce qui a été choisi
  language: 'en' | 'fr',             // ce que l'interface affiche
  system: 'en' | 'fr'                // la langue du système, pour le choix « Système »
}
```

`system`, the default, is not written to `settings.json`. It resolves in Rust, so
that the window and the tray agree: the display language on Windows
(`GetUserDefaultUILanguage`, not the regional format — someone reading Windows in
English with French dates expects English), `LC_ALL`, `LC_MESSAGES` or `LANG`
elsewhere, and English for any language the interface is not written in.

`get_language` never fails: with unreadable settings it gives the system's
language, since the window must still be able to say what went wrong. The window
reads it before mounting, so that it does not show English for a moment.

### `set_theme(theme)`

`'system' | 'light' | 'dark'`, chosen in the top bar (#110). Only `light` and
`dark` are written. Rust only saves it: the window sets `data-theme` on the
document, and `tokens.css` follows the system's setting live when there is none.
The window reads it before mounting, like the language, so that it does not show
the other theme for a moment.

### `open_log_dir()`

Opens the log folder in the system's file manager. A
log nobody knows how to find is useless, and the path depends on the
system: giving it to read is not enough.

### `diagnostic() -> string`

The text to paste into a bug report: application version, system,
known devices — plugged in, remembered decision, serial fingerprint, layout — and
engine state device by device.

For each device, **the firmware read, next to the surveyed one** — the
first field anyone will ask for in front of unexplained behavior — then the verdict
of each command and the warnings. A closed device is reported as
"non lu, appareil fermé" (not read, device closed) rather than repeating the version from a past open:
the unit plugged in since may no longer be the same. The diagnostic performs
no exchange with the device, it rereads what the open obtained.

A verified command is reported there as "connue" (known), never "understood" or "compatible":
see above what the status byte does not tell.

Cannot fail because of a device: being unable to enumerate USB or read back the
settings is exactly what a diagnostic must **say**, not what should
interrupt it.

Every path Candeo writes as text — here, in the "Candeo starting" log line, in
error messages — has the home directory as `~` (`src-tauri/src/paths.rs`): the
log and the diagnostic end up in public bug reports, and the home directory
usually carries the user's name. What follows `~` still says where the file is.
Code that opens a folder keeps the real path, and Settings shows the full path
of the log folder.

### `log_from_webview(level, source, message)`

Logs into the same file what the window sees: `app.config.errorHandler`
and effect compilation refusals, which until now went to a console that
nobody opens — and which does not exist in `release`, the binary being compiled with
`windows_subsystem = "windows"`. Target `candeo_webview`, origin as a field.

`level` excludes `trace`: per-frame output comes from the engine, not from the window.

---


## Effects engine

**One Rust render thread per device**, independent of the window: closing
the application does not turn off the effects. It is the only place where effect code
runs — the front end never runs any, which incidentally denies it any access to
the DOM and the Tauri API. Design in
[`../design/effects-runtime.md`](../design/effects-runtime.md) §4 and §5.

**One device, one effect.** Each carries its own loop, hence its own frame rate, its own
parameters, its own error state and its own output. Nothing is shared between two
devices, and that is what keeps a failing device from affecting any other.

### `start_effect(device, id, params)`

Loads the JavaScript compiled from the effect file's current bytes, refused while the file is `stale` or `broken` — and
starts the loop **of this device**. The engine makes
no difference between the two: a shipped effect is a module loaded
exactly like the one you just wrote.

Replaces the running effect **on this device**, if there was one — the previous
stop is **awaited**, otherwise two loops would briefly write to the
same device. The other devices are not affected, and the wait does not hold
them up: the awaited lock is specific to the targeted device.

A syntax error or a malformed module is reported **at call time**, not
discovered later in a state: the call waits for the load verdict.

The layout comes from **the targeted device**, open or not. Deliberate, on two
counts: a device's layout does not depend on its presence, and one must be able to
write and preview an effect **without owning the keyboard**. Targeting an
unplugged device therefore starts the effect, feeds the simulator, and leaves
`reachingKeyboard` false until the open.

#### What an effect module must expose

```ts
import { hsv } from '@candeo/effects-api'

export default {
  name: 'Mon effet',
  render({ layout, time, frameIndex, frame, params }) { … },
}
```

**A default export, and nothing else.** The import of `@candeo/effects-api` is
resolved to an internal module provided by the host: no bundler, no
`node_modules`, no path resolution.

Each frame starts from black. An effect that writes only part of the keyboard
therefore does not silently inherit the previous frame — a frame is complete by
definition.

### `stop_effect(device)` · `set_effect_params(device, params)`

`set_effect_params` adjusts **live**: the loop rereads the parameters at every
frame, it does not restart. Both touch only the targeted device; a
device on which nothing was ever started ignores them silently.

`params` **replaces** the whole table, it does not merge into it: the caller sends
the complete state, not just the field it has just changed.

The call is cheap but not free, and a slider produces dozens of them
per second. The window therefore throttles them to **25 per second at most, a single one in
flight at a time**, overwriting intermediate states: the loop only reads the
latest, and a queue would only deliver it late. The last
requested state always goes out — it is the only guarantee that matters, since it is
the one you see.

Remembering these values from one launch to the next is the job of
`remember_effect_params` (§Settings), which only writes to disk.

### `set_output_to_keyboard(device, on)`

Cuts or restores writing to **this** device, **without touching the
simulator** or the other devices. The two outputs of a loop are
independent, and each can be absent:

- window closed → only the HID write remains, no frame is serialized;
- keyboard output cut → only the simulator is fed;
- both active → the preview shows exactly the bytes sent.

### `subscribe_frames(device, channel)` · `unsubscribe_frames(device)`

A command answers **once**; an effect produces 30 frames per second. Frames
therefore travel up through `tauri::ipc::Channel`, created on the front end and passed as an
argument. They travel over it in binary (`InvokeResponseBody::Raw`):
396 bytes, against more than 1.5 KB serialized as a JSON array of integers.

One channel **per device**: the simulator follows the selected one, and
changing the selection closes one channel to open another. A subscription left
open on the previous device would feed the same simulator in parallel.

Unsubscribing stops the stream **without stopping the effect**, which keeps feeding
the keyboard.

### The preview: `start_preview` · `stop_preview` · `set_preview_params`

```ts
start_preview(device: { vid, pid } | null, id: string, params: object)
```

The exact counterpart of `start_effect`, **minus everything that commits**: no hardware
output, nothing written to `settings.json`, and above all **no device
loop stopped**.

That is what makes "selecting an effect starts the preview" possible. The engine is
one effect per device (issue #26, deliberate): previewing Y on a keyboard that
runs X would have stopped X, in other words **browsing the gallery would have turned off
the current lighting**. The preview loop is therefore separate, and its hardware
output is the one that writes nowhere — `DeviceOut::present` returning `None`
already means "no device open, this is not a failure".

`device` designates the device whose **layout the preview borrows**: it is neither
opened, nor controlled, nor necessarily plugged in. `null` falls back on the default layout
— one previews without owning a keyboard, and without having adopted any.

**There is only one.** Calling again replaces the previous one, and each
replacement destroys a QuickJS context to build another. The rate
is therefore bounded **on the gesture side** — the window waits for the selection to settle
(180 ms) — and not in the engine, which would have had to choose between making the
latest selection wait and losing it.

`set_preview_params` adjusts live, like `set_effect_params` for a device:
the loop rereads its JSON at every frame.

The preview stops: when you leave the screen, **when the window hides to the tray** (it is
the Rust side that does it, since the web view is not destroyed), when the effect it
runs is deleted, and with `stop_all` — configuration reset,
application exit. The applied effect, on the other hand, survives all of that: that is the whole
difference between what the keyboard does and what you are looking at.

### `subscribe_preview_frames(channel)` · `unsubscribe_preview_frames()`

Like `subscribe_frames`, for the preview loop. A channel **separate** from the
devices' one: both streams exist at the same time, and the window chooses which one
it draws.

⚠️ The channel lives in the loop's state, and `start_preview` builds a
new one: **you must resubscribe after every start**, exactly as for
`start_effect`. Otherwise the simulator stays frozen on the last frame of the
previous one, without any error saying so.

### `engine_status() -> EngineReport`

```ts
{
  devices: {
    device: { vid: number, pid: number },
    running: boolean,
    effectId: string | null,
    error: string | null,
    deviceError: Failure | null,   // deviceWrite, or deviceClosed: see Errors
    reachingKeyboard: boolean,
    toKeyboard: boolean
  }[],
  preview: {
    layoutOf: { vid: number, pid: number },   // gabarit emprunté
    running: boolean,
    effectId: string | null,
    error: string | null
  } | null
}
```

**Two fields, not a list with a flag.** What runs on the hardware
and what you are looking at must not be confusable: this is the fourth time
in this project that a lying state has cost a debugging session — the keyboard not
adopted, the "accepted" write, the frame frozen after automatic stop, and
now the preview. A flag to filter is filtered badly: all it takes is one caller
that forgets it to announce as running on the keyboard an effect that is only being
watched. Here there is nothing to filter.

**The system tray, the log and the gallery read only `devices`.**

`preview` carries neither `toKeyboard`, nor `reachingKeyboard`, nor `deviceError`:
a preview loop has no hardware output, and those fields set to false would describe
a failure where there is only a choice. It is `null` as soon as the preview
is stopped — "the last effect you looked at" is information for
nobody. A preview that cut out **on its own**, after thirty failed frames,
does however remain visible with its error: it is the only way to know
why the screen froze.

**A device whose writes keep failing is closed.** After one second of
consecutive failures, the loop drops the device: `deviceError` then says it was
closed and must be reconnected from the devices screen, and `list_devices`
reports it as not open. The effect keeps running. See
[`effects-runtime.md`](../design/effects-runtime.md), §7.

**One entry per device** in `devices`, `reachingKeyboard` included. A global
state would force choosing which one to show, and the next would erase the previous one
— exactly what the open failures table already avoids on the adoption side.

The list covers the devices on which an effect has been started since
startup, not only those running one right now: a stopped device
keeps its line, `running` false. "This device is doing nothing" and "I know
nothing about this device" do not mean the same thing, and the UI must be able to
tell them apart. The order is stable, sorted by VID then PID.

Polled rather than pushed: an error that occurred while the window was closed must be
readable on reopening, which a one-off event does not allow.

An exception in an effect **does not bring down the application**: it is
caught per frame, exposed here, and cleared as soon as the effect recovers. After
thirty consecutive failed frames, the loop stops — an effect that throws on
every frame will not recover on its own. And it stops only **its own** loop:
the other devices carry on.

---

## The only event: `candeo://etat-change`

```ts
listen('candeo://etat-change', () => { /* charge utile vide */ })
```

Everything else on this page is **polled**. This one is pushed, and it is pushed
for a precise reason: from the system tray icon
([`src/tray.rs`](../../apps/desktop/src-tauri/src/tray.rs)), the state can change
**without the window** — an effect started, an output cut, a keyboard turned off — and
the window no longer dies when it is closed, it hides to the tray. Its snapshot can
therefore become days old.

What it already polls every second — `engine_status` — does not need
this event. What it reads only **once**, on mount, does need it: the
device list, and `settings.json`. Probing the disk and USB in a loop to
cover a few changes per session would be the wrong trade-off.

Emitted in two cases, and the payload is empty in both: nothing says *what*
changed, because the recipient rereads everything anyway.

1. after every action of the icon's menu;
2. when the window is brought to the foreground — it is the same path as the
   second launch of the application, see `single_instance::reveal`. A window
   that has just been **reopened** does not hear it: its JavaScript is not
   loaded yet, and it reads everything on mount.

No additional permission: `core:event:default`, which `core:default`
includes, already grants `listen`.

The name is written on both sides — `tray::STATE_CHANGED` and `src/api/candeo.ts` — and
a Rust test checks the two against each other: nothing else ties them together, and letting them
drift apart would produce a window that no longer resynchronizes, without a single error anywhere.
