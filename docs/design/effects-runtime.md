# Lifecycle of an effect — from the editor to the keyboard

This document describes what happens between the moment you type TypeScript and
the moment a key lights up. The **interface** decisions are in
[`studio.md`](studio.md); these concern execution.

> **Storage has changed since.** §2 and §3 describe the first layout, one
> directory per effect written through `install_effect`, with built-ins compiled
> into the binary and a reserved built-in identifier. Effects are now `.ts`
> files named after the effect, compiled into a cache, and the manifest is read
> by running the module: see [`effects-library.md`](effects-library.md). The
> engine part is unchanged.

---

## 1. Overview

```
  ┌─ WebView ──────────────────────┐        ┌─ Rust ────────────────────────────┐
  │                                │        │                                   │
  │  Monaco (TypeScript)           │        │                                   │
  │      │ ts.transpileModule()    │        │                                   │
  │      ▼                         │        │                                   │
  │  JavaScript ───────────────────┼─ ①  ──▶│  écriture disque   effects/<id>/  │
  │                                │commande│        │                          │
  │                                │        │        ▼                          │
  │                                │        │  ┌── rquickjs ──┐                 │
  │  simulateur  ◀─────────────────┼─ ② ────┼──│  boucle 30 Hz │──▶ HID ──▶ 🖮   │
  │              (images)          │ canal  │  └──────────────┘                 │
  └────────────────────────────────┘        └───────────────────────────────────┘
```

The downward direction ① and the upward direction ② **are not the same mechanism**, and
that matters: see §4 and §5.

---

## 2. Downstream — from the editor to disk

1. **Transpilation in the front end.** `ts.transpileModule()`, provided by Monaco
   (see [`studio.md`](studio.md) §2), strips the types. Nothing more: no
   bundling, no import resolution.
2. **`install_effect` command.** The front end sends the **TypeScript source**, the
   **generated JavaScript** and a manifest (name, declared parameters).
3. **Write to disk**, under the folder described in §3.

### Why store both

| File | Role | Required? |
|---|---|---|
| `source.ts` | reopen the effect in the editor | yes, otherwise the effect can no longer be edited |
| `effect.js` | what the engine runs | **yes** |
| `manifest.json` | name, description, parameters, API version | yes |
| `swatch.json` | the library's color swatch | no — without it, a neutral dot is shown |

The `swatch.json` is the only one of the four that **nobody writes**: it is sampled
by running the effect at install time. Were it declared, it would drift from the first
code change. The mechanism is described in
[`../api/commands.md`](../api/commands.md), § "The color swatch".

The `.js` is not a cache that could be regenerated on demand: the
transpiler lives in the front end, so **regenerating would require opening the window**.
Yet an effect must be able to start without a UI — at login, or
after a reboot. The `.js` is therefore a deliverable, not a throwaway artifact.

---

## 3. Where effects live

> **Not in `Program Files`.** That folder is read-only for a
> standard account, and its contents are shared by every account on the machine. An effect
> is content **written by the user, specific to the user**.

These paths are not built by hand: Tauri's API applies each system's
convention.

| Tauri call | Windows | Linux | macOS |
|---|---|---|---|
| `app_data_dir()` | `%APPDATA%\com.o2csi.candeo` | `~/.local/share/com.o2csi.candeo` | `~/Library/Application Support/…` |
| `app_config_dir()` | `%APPDATA%\com.o2csi.candeo` | `~/.config/com.o2csi.candeo` | `~/Library/Application Support/…` |

On Windows the two coincide; on Linux they do not, hence the point of going
through the API rather than a constant.

**Chosen split:**

```
app_data_dir()/effects/<id>/     source.ts · effect.js · manifest.json · swatch.json
app_config_dir()/settings.json   préférences · appareils · effet appliqué · réglages
```

The effect is **content** (`data`), the choice of the active effect is
**configuration** (`config`). A moot distinction on Windows, an accurate one on
Linux — and free in both cases.

The adoption state of each device (§7) is configuration in the same way:
it is a decision by the user about their machine, not content you would
carry elsewhere.

> **Built-in** effects are not on disk: they are compiled into the
> binary. Only effects written by the user have a folder. Their color
> swatch therefore lives in memory, computed once per run: it is a
> property of the binary, not of the user's library.

### Built-in effects are JavaScript, not Rust

They live in `apps/desktop/src-tauri/src/builtins/`, one `.js` file each,
embedded with `include_str!` and loaded by the engine like any other effect.

Writing them in native Rust would make them faster — and prove nothing. They
are there to be read: the first example you open must be **exactly**
what you can write yourself, same API, same `export default`. An example you
cannot reproduce is not an example, it is a demo.

An accepted consequence: they have no `.ts`. Their JavaScript is their source,
so there is nothing to transpile at build time, and `read_effect_source` returns them as
they run.

The manifest, however, is written twice — in Rust so that listing the library
instantiates no engine, and in the module because that is the API contract.
A test loads each effect and compares the two; the module is authoritative.

### A built-in identifier is reserved

Built-ins share the namespace of user effects: same validation,
same `id` in `settings.json`. Two safeguards, in this order:

1. `install_effect` **refuses** a name that derives to a built-in identifier;
2. the `id → JavaScript` resolution checks built-ins **first**.

The second is only useful if the first has been bypassed — a folder copied by
hand, a library inherited from a version where the identifier was free. The
direction of the priority follows from what we refuse: an entry marked `builtin`
in the gallery must run the shipped code. The reverse priority would let a
user effect slip in under a known name, with the built-in's manifest displayed
and other code running.

---

## 4. The engine — one loop per device, two outputs each

One Rust thread per device, independent of any window. Each instantiates its own
`rquickjs` context, loads its `effect.js`, and calls its render function at a
fixed frame rate.

```
   appareil A                              appareil B
        ┌──────────────────────────┐            ┌───────────────┐
        │  rquickjs — render(ctx)  │  30 Hz     │  rquickjs — … │
        └────────────┬─────────────┘            └───────┬───────┘
                     │  image : 132 triplets RGB        │
          ┌──────────┴───────────┐                      ▼
          ▼                      ▼                   sortie A'
   écriture HID            canal → simulateur
   (si ouvert et           (si la fenêtre écoute
    « envoyer » actif)      **cet** appareil)
```

The two outputs are **independent**, and either one can be absent:

- window closed → only the HID write remains; no frame is serialized;
- "envoyer au clavier" (send to keyboard) toggle disabled → only the simulator is fed,
  which makes it possible to write an effect **without owning the keyboard**;
- both active → the preview shows exactly the bytes sent.

It is this last property that justifies the single engine — in the sense of: a single
place where effect code runs. The simulator does not interpret the code, it
displays the result.

> ⚠️ **"Window closed" is nothing to take for granted, and has not always held.** A
> thread independent of the window does not outlive the process, and the process
> used to stop with its last window — nothing prevented
> `RunEvent::ExitRequested`. What this paragraph describes has therefore only been true
> since issue #46: the tray icon holds back the exit, and
> the window's close button **collapses to the tray** instead of quitting. "Quit Candeo", in
> the icon's menu, is the only thing that stops a render loop by
> ending the process — and it leaves the lighting as it is rather than
> turning it off. See [`src/tray.rs`](../../apps/desktop/src-tauri/src/tray.rs).

### One device, one effect

Each device carries its own loop, hence its own frame rate, parameters, error
state, output and `reachingKeyboard`. **Nothing is shared between two
devices**, and that is what ensures a failing device affects no
other — the adoption invariant (§7), held this time while running and not
only at open.

The other model — one effect spanning several devices, with a composite
layout — would allow a wave traveling from the keyboard to the mouse pad. It
first requires describing where the devices sit relative to one
another, which cannot be designed properly with a single device at
hand.

**It is not ruled out for all that.** A loop receives **a layout** and **an
output**, never "a keyboard": the `DeviceOut` trait is the only point where it
touches hardware. The day a layout spans several devices, that is
where the frame will be split — neither the loop nor the effect code will have to change.

It is also this seam that makes the invariant verifiable: a test makes
**all** writes to one device fail and observes that its neighbor keeps its loop,
its frames, its clean state — and that stopping the first does not stop the second.
With `Keyboard` hard-coded, this would only have been verifiable with two keyboards on
the desk, so never.

### Lock acquisition order

A deadlock has already been caught: `list_devices` took the keyboard lock
then the failures lock, `ignore_device` the reverse. With a device table and N
loops, the rule is explicit:

> **No code holds two locks at the same time.** A table is locked for as
> long as it takes to read or store a shared pointer in it — never for the duration of an
> HID write, a loop start or a wait for completion.

Where two would become unavoidable, the order is that of declaration in
`AppState`: device table → engine → failure table → a device's
handle → a loop's shared state.

Two consequences, and they are not cosmetic:

- **the render thread only knows the last two.** It has no way to
  take an application lock, hence no way to block a command with it;
- **the only wait held under a lock is `stop`'s, and that lock
  belongs to the device.** Stopping waits for the loop to finish — otherwise chaining
  two effects would briefly leave two loops writing to the same device —
  but that wait holds back no command targeting the others. An
  HID write blocked on one would not freeze the other, which a global engine lock
  would have reintroduced through the back door.

### Import resolution

`import { hsv, mix } from '@candeo/effects-api'` is resolved by the module
loader of `rquickjs` to an **internal** module, provided by the host. No
bundling, no path resolution, no `node_modules`.

The module declares the API version it was written against (`apiVersion`, a
whole number, 1 when absent): that is what makes it possible to cleanly refuse
an effect written against an API this version does not know, rather than
letting it fail at the first frame.

---

## 5. Upstream — a channel, not a global event

Tauri offers two directions, and they do not have the same primitives:

| Direction | Primitive | Shape |
|---|---|---|
| front end → Rust | **command** (`invoke`) | request / response, awaited |
| Rust → front end | **event** or **channel** | push, no response |

A command cannot "return" 30 frames per second: it responds once.
The upstream path therefore goes through a channel — `tauri::ipc::Channel`, created by the front end and
passed as an argument to a subscription command, along with **the device whose
frames are wanted**. The simulator follows the selected one; changing the selection
closes one channel and opens another, rather than multiplexing a single stream.

**Channel rather than global event** (`emit` / `listen`) for three reasons:

1. no broadcast to every window nor lookup in a registry
   of listeners — the destination is known;
2. the scope is explicit: once the channel is released, the stream stops. No subscription
   leak when the editor closes;
3. it carries binary. `InvokeResponseBody::Raw` avoids serializing a
   frame as a JSON array of integers, which would take it from **396 bytes to more
   than 1.5 KB of text** — for nothing, 30 times per second.

For scale: 132 LEDs × 30 frames/s = **3,960 colors per second**.
Choosing the channel is not a necessary optimization, it is simply the
right primitive for a stream; might as well use it.

---

## 6. Portability of hardware access

`hidapi` covers Windows, Linux, macOS and illumos — writing a feature
report works the same there. What changes from one system to another:

| | Windows | Linux |
|---|---|---|
| Default backend | hidapi C (`hid.dll`) | `linux-static-hidraw` (compiles C, links `libudev`) |
| Pure Rust backend | `windows-native` | `linux-native` (crate `udev` + `nix`) |
| Unprivileged access | immediate | **udev rule required** |

> **Compiled, or assumed?** The repository tells the two apart. A `linux` CI job
> builds the full workspace on `ubuntu-latest`: what passes there
> is *compiled*. The rest — everything that requires a keyboard plugged into a
> Linux machine — remains *assumed*, and is flagged as such below.

### What CI establishes

The `linux` job (`.github/workflows/ci.yml`) does, in this order: Tauri 2's Debian
system dependencies plus `libudev-dev`, `cargo check --workspace
--all-targets`, `cargo test --workspace`, then `tauri build --debug --bundles
deb,rpm` and a read of the two packages produced.

It therefore establishes that:

- the `linux-static-hidraw` backend compiles and links;
- no part of the workspace — `candeo-protocol`, `candeo-device`,
  `candeo-desktop` — depends on Windows to compile;
- the tests pass identically on a case-sensitive file
  system;
- the application packages as `.deb` and `.rpm`;
- the udev rule is **actually present** in both, at
  `/usr/lib/udev/rules.d/60-candeo.rules`. The check reads the packages
  (`dpkg-deb -c`, `rpm -qpl`), it does not reread the configuration — `deb` and
  `rpm` being two separate declarations, checking only one would let
  the other go wrong silently.

### What remains assumed

There is no USB behind a GitHub runner. Still to be confirmed on a
Linux machine fitted with the keyboard:

- that `send_feature_report` succeeds through hidraw on this device — the API is
  the same, the kernel path is not;
- that `interface_number` does distinguish the interfaces of the composite device. The code
  picks its device on that basis (§ `Keyboard::open`), and opening the wrong
  interface gives, on Windows, a valid handle on which every write
  fails. The hidraw backend reads the `bInterfaceNumber` attribute of the USB parent, which
  should give the same value — *should*;
- that `app_data_dir()` and `app_config_dir()` do land in
  `~/.local/share/com.o2csi.candeo` and `~/.config/com.o2csi.candeo`. That is
  what Tauri documents, and the code builds no path itself (§3),
  but nothing here has observed it.

### The udev rule, and who ships it

`/dev/hidraw*` is created as `0600 root:root`. The rule is in the repository at
[`packaging/linux/60-candeo.rules`](../../packaging/linux/60-candeo.rules):

```udev
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="1532", MODE="0660", TAG+="uaccess"
```

Three points that are not details:

1. **`uaccess` rather than a group.** systemd-logind sets an ACL for
   the user of the active local session and removes it at logout. A
   fixed group (`plugdev`) would grant access permanently, including to a remote
   session.
2. **The 60 prefix.** The rule that applies the ACL is `70-uaccess.rules`: a
   file numbered above 70 would set the tag too late and would do
   nothing.
3. **`/usr/lib/udev/rules.d/`, not `/etc/`.** The package is a vendor;
   `/etc/udev/rules.d/` belongs to the administrator, who must be able to override
   us. It is also where the file must be copied by hand when
   running Candeo from source.

Delivery goes through `bundle.linux.deb.files` and `bundle.linux.rpm.files` in
`tauri.conf.json`. The key is the path **inside the package**, the value the source
path **relative to `tauri.conf.json`** — not the other way round.

### `*-native` backends: evaluated, not adopted

The intent was to remove the dependency on a C compiler to simplify
cross-compilation. Reading the `build.rs` of `hidapi` 2.6.7 does not back it
up:

- **The gain is partial.** `linux-static-hidraw` does
  `pkg_config::probe_library("libudev")`; `linux-native` relies on the crate
  `udev`, that is, a binding to that same `libudev`. Switching to the
  native backend therefore does not remove `libudev-dev` from the dependency list,
  only the call to `cc`. Only `linux-native-basic-udev` would do without it,
  via `basic-udev` — a crate at 0.1.
- **The C compiler is required anyway.** `rquickjs` compiles the
  QuickJS C sources; `cc` is in `Cargo.lock` for that reason alone. The
  dependency we wanted to remove would not go away.
- **Cross-compilation is not simplified for the application.** On
  Linux, Tauri links to webkit2gtk, GTK 3 and libsoup through `pkg-config`: a full
  sysroot is already needed. The gain would only concern a headless use of
  `candeo-device` alone.
- **The cost is not zero.** The `build.rs` of `hidapi` aborts if two
  Linux backends are enabled ("Exactly one linux hidapi backend must be selected").
  Adopting `linux-native` therefore forces `default-features = false` in both
  manifests that declare `hidapi`, and re-listing by hand the defaults of the
  other systems.
- **`windows-native` is out of the question for now.** Windows is the only
  system where the protocol has been validated on the hardware. Changing its backend
  would trade a verified path for an unverified one, for zero gain:
  MSVC is already there, the Rust toolchain requires it.

**Decision: we keep the default backends.** CI proves that the default
compiles on Linux. To be reopened if a headless static binary is wanted —
that is the only case where the calculation would change.

---

## 7. Device-by-device adoption

You had to click "Connecter" (Connect) at every launch, and not doing so
produced **no** sign: the simulator animated, the "envoyer au
clavier" box stayed checked, the keyboard kept its frame. That silence cost an
entire round of debugging — the protocol, the frame rate, the
engine were all suspected, before finding that nothing was open.

The answer is not to open everything that is detected.

### Why not simply connect everything

Writing to a USB device you understand poorly is not harmless, and at
the scale of a growing catalog — keyboards, mice, memory, fans —
adopting by default is how you break someone's hardware. There are also
devices you do not *want* to see controlled: a vendor driver already in
place, or a model whose survey is uncertain.

### Three states, decided once and remembered

| State | At launch |
|---|---|
| `adopted` — controlled | opened automatically, without asking anything |
| `detected` — detected | listed, but **not** opened |
| `ignored` — ignored | left alone, and stays that way |

The default is `detected`: the ceremony disappears without anything being taken
over without consent. The exact shape in `settings.json` is in
[`../api/commands.md`](../api/commands.md).

### Identity rests on VID / PID / serial, nothing else

The same keyboard reported itself as `v1.4 / Unkown Variant` then `v1.5 / Quartz`
during the protocol survey. A binding that matches on the variant or the
firmware therefore breaks at the update, and the adopted device becomes an
unknown again — which is exactly the ceremony just removed.

The serial number is only compared if **both sides** carry one: it tells apart
two units of the same model, but a silent enumeration — hidraw without a
udev rule (§6) — must not unpair an already adopted device.

### One device failing takes no other down

This is the startup invariant, and it is verified rather than assumed. The open
loop knows neither Tauri nor HID: presence and opening reach it
as arguments, which makes the invariant testable with an ordinary test.
`a_failing_device_blocks_no_other` makes opening the
first of two controlled devices fail and checks three things:

1. the loop went on to the second, which is indeed open;
2. the failure message stayed on the first;
3. the second's report is clean — the error did not spill over.

In operation, these messages live in a table indexed by VID/PID, and
`list_devices` gives each device its own. A single field would force choosing which one to
display, and the next would overwrite the previous.

**The invariant also holds while running, not only at open.** A device
that was opened successfully can very well refuse every write afterwards — unplugged,
put to sleep, preempted by a vendor driver. `a_broken_device_affects_no_other`
starts two real loops, makes all writes of one fail, and checks
that the other keeps its loop, its frames and its clean state — and that stopping the
first does not stop the second. See §4.

### What this implies for the state

`AppState` holds a **table** of open devices, indexed the way adoption
identifies them (issue #26). Adoption is multiple, and so is the open handle:
every controlled and present device is opened at startup, each with its own
loop and its own effect.

Closing a device — ignored, unplugged — empties its handle without removing it from the
table. The loop that fed it holds a copy: it notices at the
next frame, stops writing, and says so through `reachingKeyboard`. That is what
makes it possible to stop writing to a device without stopping the effect running on it.

**The loop also closes a device on its own** after one second of consecutive
failed writes (`MAX_DEVICE_WRITE_ERRORS`). An unplugged keyboard leaves a dead
handle: plugging it back in creates a new device instance that this handle never
reaches again, and keeping it made the window and the tray report the device as
open while nothing got through (#72). The close happens under the handle's lock,
in the same critical section as the failing write, so it cannot drop a keyboard
that a reconnection has just put back. The effect keeps running; reconnecting
goes through the existing commands.

---

## 8. Still to do

- [x] `install_effect`, `list_effects`, `delete_effect` commands
- [x] `start_effect`, `stop_effect`, `set_effect_params` commands
- [x] Subscription command returning frames through `Channel`
- [x] `rquickjs` render thread + `@candeo/effects-api` internal module
- [x] Reading and writing `settings.json`
- [x] Built-in effects, written against the public API
- [x] Color swatch sampled from the render, at install time
- [x] udev rule, shipped by the `deb` and `rpm` packages
- [x] Linux build and packaging verified in continuous integration
- [x] Device-by-device adoption, and opening of controlled devices at startup
- [x] One effect per device: table of open devices, one loop each
- [x] Removing an effect from the library, and resetting the
      configuration (`reset_settings`) — two distinct actions, one on
      content and the other on configuration, both of which stop
      the affected loops **before** writing
- [x] Execution bounds for an effect: a compute time per frame and at
      load, a memory limit per effect — a `while (true)` or an array that
      grows every frame becomes a frame error, not a freeze
- [x] Resuming the active effect at startup and on adoption (#102)
- [ ] Verification of the `hidraw` backend **on hardware** — feature report
      writes, filtering by `interface_number`, paths resolved by
      Tauri. Requires a keyboard plugged into a Linux machine.

### What the implementation clarified

- **The manifest is read by running the module**, once, in Rust, under the load
  budget, when the library compiles it (`effects-library.md` §3). It was first
  read from the syntax tree by Monaco's compiler, which required literals; that
  reader is gone, and defaults may be computed.
- **An effect exports by default.** The glue imports the namespace rather than
  the default export: `import effect from 'effect'` fails at module *linking*
  when it is missing, with a QuickJS message that cannot be tied to
  any line of your own code.
- **Each frame starts from black.** An effect that writes only part of the keyboard
  does not silently inherit the previous frame: a frame is complete by
  definition.
- **Colors are clamped on the JavaScript side**, not only in `rgb()`:
  nothing forces an effect to go through the API, it can build `{r, g, b}` by
  hand. Without this, it is the Rust-side conversion that fails — far from the cause.
- **The loop targets an absolute deadline**, not `sleep(période)`: a
  slow frame must not shift all the following ones. When running late, it restarts from
  now rather than catching up at high speed.
- **An exception kills nothing.** It is caught per frame and exposed through
  `engine_status`, then cleared as soon as the effect recovers. After thirty
  consecutive failed frames, the loop stops.
- **The color swatch comes from the engine, not from the manifest.** Four frames at
  irregularly spaced instants, and in each the average of a diagonal
  band that moves forward: that is what keeps a spatial effect and a uniform
  effect from looking alike. Sampling always at the same spot would
  confuse them, and so would an average of the whole frame.
- **Everything that runs effect code is bounded**, in time as in memory.
  This was first true of sampling alone, because a `while (true)` there
  prevented an installation from completing. It is now true of the render
  loop, where the lack of a bound was worse: the `stop` flag is read *between* two
  frames, a render that never returns never reads it again — and since an effect
  runs with the window closed, closing the window does not rescue it. Since the close button collapses to the tray instead
  of quitting, it rescues it even less: the last resort is
  "Quit Candeo" in the icon's menu, which takes down the whole process.
- **The two bounds do not have the same shape**, because the interrupt
  handler is set on the `Runtime` **once**. Sampling only
  needs one deadline, captured by value; the loop changes it every
  frame and therefore shares a cell that it renews before each call. That
  cell also carries what is needed to know that it really was the deadline that cut in:
  QuickJS raises "InternalError: interrupted" whatever the reason.
- **An overrun is an ordinary frame error.** It takes the
  exception path above, the one that thirty consecutive failed frames
  turn into a clean stop — no new path. The engine only adds
  the name of the cause, in French: "l'effet a dépassé son temps de calcul" (the effect exceeded its compute
  time) can be tied to its code, a QuickJS error cannot.
- **The budgets are measured.** An ordinary effect costs 0.23 ms per frame in
  `release`, a field of five thousand particles with a one-second trail
  1.1 ms. The 10 ms budget is therefore not a performance allocation but
  **a freeze detector with headroom for machine hiccups**: the deadline is
  measured in wall-clock time, not CPU time, and a thread suspended by
  the scheduler consumes its budget without executing anything. The ceiling comes
  from elsewhere: the HID write takes 14.4 ms at worst within the same
  33.3 ms period, and the deadline would only be missed silently from about 19 ms
  of budget upward. Memory does not depend on the profile: 32 MB, five times the most
  outlandish stateful effect we know how to write for 132 LEDs.
- **The debug budget is separate — 200 ms — and it is a ceiling, not a
  target.** QuickJS is C compiled at the profile's optimization level: un-optimized,
  the same frame went from 0.23 ms to 6.3 ms. Since
  `[profile.dev.package."*"]` sets dependencies to `opt-level = 2`, QuickJS
  is optimized in debug too and the gap should have melted away — **to be
  re-measured** before unifying the two values.
- **A device gets a status line as soon as it has carried an effect, and keeps it.**
  `engine_status()` returns one entry per targeted device, `running` false once
  the effect is stopped. Removing the line would make "this device is doing nothing"
  indistinguishable from "I know nothing about this device".
- **Stopping waits for completion, per device.** The lock waited on is that of
  the targeted device, never the engine's: an HID write blocked on one must
  not hold back commands targeting the others, otherwise the invariant of §7
  would be lost one level up.

### The only test that crosses the whole chain

Everything else is verified without hardware: the reports, the matrix, the engine,
the built-in effects. Still, none of those tests proves that a byte reaches
the keyboard.

```
cargo test -p candeo-desktop bout_en_bout -- --ignored --nocapture
```

`end_to_end_on_the_real_keyboard` opens the device, runs a built-in
effect for three seconds on **this device**, checks that the loop holds, that no
frame threw and that frames do reach the keyboard (`reachingKeyboard`),
then stops. Marked `#[ignore]`: it requires a plugged-in keyboard, so it has no place
in continuous integration.

**It really writes to the keyboard** — that is the point, and it is visible.
