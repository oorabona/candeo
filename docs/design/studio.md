# Interface design — validated for v1

**Reference mockup**: not published

This document freezes the decisions made before writing a single line of Vue. It
exists so that the implementation has a target, not to describe what exists.

---

## 1. The gallery is the home screen

The app does **not** open on an editor. It opens on the list of available
effects, and a "＋" button enters editing. This list has become a
three-column screen: see §8.

The reason: most launches are for choosing an effect, not for writing
one. Making the editor the home screen would impose a development tool on
someone who just wants to change color.

### Three kinds of effect, visually distinct

| Kind | Origin | Cost | Survives closing |
|---|---|---|---|
| **Built-in** | shipped with the app, copied into the effects folder | host loop | yes, via the service |
| **Yours** | written in the editor, saved | host loop | yes, via the service |
| **Hardware** | keyboard firmware | **none** | **yes, always** |

The distinction is not cosmetic. A hardware effect (`Spectrum Cycle`, `Wave`)
keeps running with the computer off and costs no processor time; it is
often the right choice, and the interface must say so.

---

## 2. The editor is a mode, not a separate screen

Opening "＋" replaces the window content: editor on the left, keyboard
simulator on the right, always visible.

### Monaco, not CodeMirror

Chosen for its **TypeScript language service**: by loading the `.d.ts` of
`@candeo/effects-api`, you get types, autocompletion and inline errors without
writing anything specific. Weight does not factor in — the app is
packaged, there is no download at use time.

A second benefit, found afterwards and decisive for §3: Monaco does not ship
a home-made parser but **the TypeScript compiler itself**. Its
`ts.worker.js` re-exports `typescriptServices` under the name `ts` — 6.6 MB of
minified compiler, already paid for by the editor. `ts.transpileModule()` is therefore
available without adding anything.

> The import goes through the direct path of `typescriptServices` and not through
> `ts.worker.js`: the latter sets `self.onmessage`, which a module loaded in
> the window has no reason to do. Its **types**, on the other hand, come from the
> `typescript` package, already there for `vue-tsc` — only its declarations are read, it
> weighs nothing in the build.

### How the declaration reaches the language service

`?raw` reads `packages/effects-api/src/index.ts` **at build time** and embeds it
in the bundle as a string; `addExtraLib` places it at
`file:///node_modules/@candeo/effects-api/index.ts`, and `paths` points the
package name there. The file therefore remains the **single source**: no copy, no
generated `.d.ts`, no build step to keep up to date.

Monaco does not actually require a declaration — `addExtraLib` accepts any
TypeScript, and the service derives the same thing from it. Function bodies
included, which is better: the tooltip for `hsv` then shows the real code.

Verified by reconstructing the worker host with the compiler that Monaco
ships: on the starter template, **no diagnostics**; the tooltip on `hsv`
renders `(h: number, s: number, v: number): Rgb`; a typo is
flagged. The whole chain is therefore checked, not assumed.

### Neither DOM nor Node in the editor

`lib: ['es2020']`, and nothing else. An effect runs in QuickJS: there is no
`document`, no `fetch`, not even `console`. Offering them in autocompletion would
promise what the engine does not provide, and the error would only show at the
first frame.

`strict` in full, `noImplicitAny` included. The check does not bend, and the author
has no incantation to write — **`defineEffect` solves both**.

An object literal has no contextual type: written bare, its `render({ layout,
time, frame, params })` are implicitly `any`, and `strict` rejects them.
Four errors, on the most natural way to write an effect.

Two wrong answers were tried before the right one:

1. **Disable `noImplicitAny`.** The effect passes, but `layout` becomes `any`
   silently: autocompletion is removed in the one case where it is missing,
   that is, the reason for choosing Monaco is given up to avoid an
   error message.
2. **Require `satisfies EffectModule`.** The typing is correct, but it is a
   ceremony the author must know about, and forgetting it produces an
   incomprehensible message. Above all, it was **impossible** for built-in
   effects: they are JavaScript files the engine runs as-is,
   where `satisfies` would be a syntax error. Opening them in the editor
   therefore gave four errors, with no recourse — the "start from an effect
   that works" path was broken from the outset.

`defineEffect` is the identity at run time: it costs nothing, remains valid
JavaScript, and provides the contextual type. Better, it **infers the type of
each parameter from its declaration** — a `color` parameter arrives as `Rgb`,
a `number` as `number`, with no cast, which would in any case be
impossible in a file run as-is.

Verified with the compiler shipped by Monaco: the API, the starter template and
the five built-in effects produce **zero diagnostics** under full `strict`.

This is also what constrains the shape of the API. Key geometry is
**optional** — `geometry` capability, [`device-sdk.md`](device-sdk.md) §3.2 —
so `key.x` is `number | undefined`, and `key.x - cx` would be a diagnostic in
a built-in effect opened here. Hence `center` and `bounds`: they return a
non-optional rectangle, or throw naming the key that has none. A
spatial effect therefore stays readable **and** clean under `strict`, without having to
pretend that every layout is drawn.

### Workers are bundled, not downloaded

Monaco loads its workers through `new Worker(...)`. Vite's `?worker` turns them into
project assets, in development as in the shipped app. No CDN:
the app opens offline, which is the least one can expect of a
desktop application.

### What Monaco weighs

| | full build | entry chunk |
|---|---|---|
| before | 354 kB | 87.4 kB |
| after | 14.5 MB | 89.7 kB |

Most of the weight is the TypeScript worker (6.9 MB) and the compiler loaded
at save (3.5 MB). But **the entry chunk does not move**: the editor
sits behind a lazy-loaded route, the gallery pays nothing. It is the
only one of the two figures that matters — the app is packaged, there is no
download at use time.

### An effect being written is not lost

Until it is saved, an effect exists nowhere: saving is the only path to its
file, and it requires code that compiles. Yet you leave
the editor long before getting there. The editor therefore saves continuously to the
web view's local storage, under one key per effect, and restores on opening.

No "do you want to save?" dialog: it asks a question
that can be answered wrongly, and only once. A restored draft
loses nothing, asks nothing, and is discarded with a button. Text identical to the
saved version is not a draft, either: it is cleared, otherwise
the editor would announce a restore that restores nothing.

The durable copy, for its part, remains the `source.ts` written at install.

### The simulator is permanent, and saving previews

Iterating on an effect must not require looking at the real keyboard, or
owning one, and it must not take over the lighting in use. The editor therefore
follows the gallery's rule (§8):

- **Save** installs the effect, then restarts it in the **preview loop**, which
  writes to no keyboard.
- **Apply on *device*** starts it on the current device for real, saving first
  when the code differs from the saved version. Its parameters follow the
  gallery's Apply: the manifest defaults, overridden by what is remembered for
  that device.

This replaces a Validate and start button next to a Send to keyboard toggle,
checked by default: every
validation replaced what the keyboard was running, and the gallery and the
editor followed opposite rules for the same gesture.

The simulator shows the device's frames when the device runs **exactly the
saved version**, and the preview otherwise — one composable, shared with the
gallery, decides it. Both loops load the JavaScript compiled from the saved file, so unsaved code is
never on screen. The engine reports which effect a device runs, not which
version of it: the editor remembers the text it applied, otherwise saving an
applied effect would keep showing the device's stale frames instead of the new
code.

The simulator renders the **physical drawing** of the keys (see §4), not
the logical grid: a spatial effect cannot be judged on a regular grid.

But the simulator **computes** nothing: it receives frames already produced by
the runtime engine — the preview loop's, or exactly the ones going to the
keyboard. That is the whole point of §3.

---

## 3. A single runtime engine, on the Rust side

> This section says **why**. The detailed lifecycle — on-disk storage,
> render loop, frame streaming — is in
> [`effects-runtime.md`](effects-runtime.md).

### The starting question was the wrong one

It was: will TypeScript at run time handle the load?

**Yes, easily.** 132 LEDs × 30 frames/s = **3,960 colors per second**. That is
trivial for any JavaScript engine, and so is the IPC crossing per frame.
Performance is **not** an issue, and never was.

Two real constraints, however, drive everything:

1. **An effect must run with the window closed.** As long as rendering lives in the
   WebView, closing the window turns the keyboard off.
2. **The preview must be production, not a likeness of it.**

> ⚠️ **Moving rendering out of the WebView is necessary, and not sufficient.** A thread does not
> outlive the process, and the process stopped with its last window:
> all that time, constraint no. 1 was an argument upheld by the
> design and contradicted by execution. What actually upholds it is
> the system tray icon (issue #46): it intercepts
> `RunEvent::ExitRequested`, and the window's close button **hides** it instead of
> quitting. See [`src/tray.rs`](../../apps/desktop/src-tauri/src/tray.rs).
>
> The two halves cannot be undone one without the other. Removing the icon means
> making everything this paragraph justifies false again.

### The rejected version, and why

The first draft of this document proposed this:

```
Monaco (TS) ─esbuild-wasm─▶ JS ─┬─▶ exécuté dans le WebView  ─▶ simulateur
                                └─▶ IPC ─▶ rquickjs (Rust)   ─▶ clavier
```

**Two different JavaScript engines run the same effect** — V8 in the
WebView for the preview, QuickJS in production. The simulator then does not show
what the keyboard does: it shows what another engine would do with the same code.
The gap is discovered late, on one specific effect, and it is painful to diagnose.

Adding `esbuild-wasm` to serve this scheme meant paying for an extra
transpiler to get a defect.

### What we do instead

```
Monaco (TS)
   │  ts.transpileModule()          ← déjà embarqué dans Monaco, rien à ajouter
   ▼
  JS ──── IPC install_effect ────▶  rquickjs (Rust)   ← moteur unique
                                          │
                         ┌────────────────┴────────────────┐
                         ▼                                 ▼
              événement « frame » → simulateur      écriture HID → clavier
```

- **Transpilation is free.** `ts.transpileModule()` strips types,
  nothing more; it is a text-to-text function, deterministic, with no
  runtime semantics. So it does not matter where it happens — and Monaco already
  provides it. **Neither `esbuild-wasm`, nor `oxc`, nor `swc`: none of the three is required.**
- **`rquickjs`** runs the JavaScript on the Rust side, in a thread independent of the
  window. A single engine, so the preview **is** production, by construction.
- **The front end never runs user code.** A safety benefit that comes
  for free: an effect can touch neither the DOM nor the Tauri API.
- Imports from `@candeo/effects-api` (`hsv`, `mix`…) are resolved by the
  `rquickjs` module loader to an internal module. **No bundler.**

The simulator costs 396 bytes per frame, 30 times per second — the figure already
judged trivial above. It travels over a **channel**, `tauri::ipc::Channel`, and not
a global event: the scope is explicit and the binary passes raw, with no
detour through a JSON array of integers. Details in
[`effects-runtime.md`](effects-runtime.md) §5.

> `boa_engine` (pure Rust, no C) would be simpler to embed, but QuickJS
> is noticeably faster and more complete. For arbitrary user code,
> language coverage matters more than the absence of FFI.

**AssemblyScript / WASM remains rejected**: it is not TypeScript but a
subset, and the user discovers its limits after writing their
effect. The learning cost is offset by no gain we need.

---

## 4. The keyboard must look like the keyboard

The mockup draws the **real full-size ISO** layout: L-shaped Enter,
separate numeric keypad, 6.25-unit space bar, inverted-T arrow keys.

> **The device does not declare its geometry.** It only exposes the logical
> 6 × 22 grid. The realistic drawing is therefore **written by hand** from the
> standard ISO layout, and the cell ↔ key mapping comes from the survey
> documented in [`../protocol/deathstalker-v2-pro.md`](../protocol/deathstalker-v2-pro.md).

Consequence: a new keyboard model will require a drawing, not just a
matrix. This is deliberate — the alternative, a grid of squares, makes the simulator
useless for judging an effect.

---

## 5. What was removed

**The display of the current frame** (the 132 triplets in plain text) was removed from
the mockup. It is a debugging tool, not information useful in
everyday use; its place is behind a diagnostic mode, not on the main screen.

---

## 6. Device selection

Only one model is supported today, but the selection screen exists
from v1: the `list_devices` command already returns every known layout with
a presence indicator, and the interface must show an absent device
as absent, not omit it.

---

## 7. Still to do for v1

- [x] Device selection screen
- [x] Monaco editor + declaration of `@candeo/effects-api`
- [x] Simulator — full-size ISO drawing, fed by the engine's frames
- [x] Saving: `ts.transpileModule()` → `install_effect` command
- [x] `rquickjs` engine on the Rust side: render thread independent of the window,
      internal module `@candeo/effects-api`, frame channel
- [x] Main screen — three columns, starting an effect from the list,
      preview in the right-hand panel (§8). The color swatch replaces the
      animated thumbnail: it is sampled from the effect's rendering, so it cannot
      describe anything other than what the effect does.
- [x] Tuning of declared parameters — one control per kind, generated from
      the manifest, adjusted live and remembered per device and per effect (§9)

No new dependency on the Rust side apart from `rquickjs`, none on the front end apart from
`monaco-editor`.

### What the implementation clarified

- **The manifest is read by the engine, not by the window.** `description` and
  `params` were first extracted from the syntax tree, which required literals.
  They are now read by Rust, which runs the module once when its compiled
  JavaScript is recorded: the window still runs no effect code, and defaults can
  be computed ([`effects-library.md`](effects-library.md) §3). The name is the
  file name.
- **Saving restarts the preview explicitly.** Nothing the preview depends on
  changes when an effect is saved again — same id, same device —, only the
  JavaScript compiled from its file does. Without an explicit restart, the simulator would
  keep running the previous code.
- **The frame channel only exists during an effect, and it targets a loop.** It
  is placed in the state of the running loop — **a device's**, or the
  preview's — and each `start_effect` or `start_preview` creates a new one: the
  simulator resubscribes after each start, not once and for all at opening.
  Switching source closes the previous channel — the engine would not replace
  it, and both streams would feed the same simulator.
- **Leaving the editor stops the preview, not the applied effect.** Nobody is
  left to watch the preview. The released channel cuts the frame stream; the
  device loop keeps feeding the keyboard, window closed included —
  the close button hides the app to the system tray, it does not quit it. Quitting it is "Quit Candeo" in the icon menu, and it is the
  only action that stops effects.

---

## 8. The main screen has three columns

**Devices · Effects · Settings.** The hierarchy is the one you
think in: you pick a device, then its effect, then its settings.
**Assignment stops being a checkbox at the bottom of a panel — it becomes
the structure of the screen.**

The thumbnail grid that occupied this screen went away with it: it
presented effects without saying what they would apply to, which only made
sense as long as there was only one possible device.

### The first two columns collapse, the third does not

With a single controlled device, a whole column would be a permanent dead
strip, and it is the preview that needs the width. The third does not
collapse: it is the content, nothing would be left.

> **The trap, and the form that resists it.** Collapsed, an entry must show
> only its icon — or its color swatch. The natural way to write it,
> listing what is hidden ("hide the name, hide the effect"), has already produced here
> a specificity collision: a rule added elsewhere for the name
> won over the hiding, and the text came back overflowing into 40 px.
>
> The form kept is the reverse: **hide all children with the universal
> selector, then explicitly restore the only one that remains**, and guard with
> `:not(.shut)` any rule that could compete with them — it *does not
> apply* in the collapsed state, instead of winning or losing an arbitration. A child added
> tomorrow is therefore hidden without anyone having to think about it. The same form is used
> twice: on the children of an entry, and on the children of the column
> body.
>
> Collapsed widths, for their part, are **variables** and not competing
> rules: each state writes its own, and the narrow breakpoint
> redefines the grid once and for all.

### The devices column

It lists **controlled** devices. Adoption stays in the
"Périphériques" (Devices) view: choosing what you configure and choosing what Candeo is allowed to
control are two different actions, and merging them would turn a selection click
into a takeover.

It shows the **product name**, not a category — that is what distinguishes
two keyboards of the same brand. It wraps rather than being truncated.
Below it, the effect the device is running.

**No manufacturer logo**: they are protected trademarks, they do not help
tell a keyboard from a mouse, and the name already carries the information. A
type pictogram is enough — and it is not guessed from the name: only one layout
is known, it is a keyboard; the day the Rust side declares a type, it will come from
there.

The **brightness** of the selected device lives in its card, under the name
and the state line: a Brightness label, the level in percent, and the slider. That is
where it belongs: the protocol makes it a device command (`0x0f`/`0x04`),
separate from the running effect, and `set_brightness` has taken a `DeviceRef` since
day one. It existed, it was persisted, and it was displayed
nowhere — it was not a display bug, it was an interface that had
never been written. Remembered per device, and **reapplied on plug-in**: a
level that is not reapplied is useless, and the surveyed protocol can
write brightness but not read it back.

Only the selected card carries a slider. The card is therefore a container
holding a selection button with the slider below it, not a button: a control
nested in a button is invalid markup, and dragging would re-select the device.
When the device is not open, the slider is disabled and the card shows the
*not open* state. In the collapsed column the card shrinks to its icon
and the slider goes with the rest.

### The effects column

Three sections, in this order: **Hardware**, **Built-in**, **Yours**. Hardware comes first: it costs no processor
time and survives everything, which often makes it the right choice (§1).

**The selection follows the selected device** (#118): when the screen opens and
when another device is chosen, it becomes the effect running on that device,
else the one it remembers, and its entry is brought into view, unfolding its
section for the visit. The engine refresh never moves it: an effect clicked
since stays selected. The screen selects nothing until the library and the
engine status are read, rather than flash the first effect and start its
preview. A device with no effect falls back to the first built-in effect, since
a hardware effect would give the simulator nothing to animate.

Each section header is a button that folds its section. Sections are expanded
by default, and the choice is remembered per viewer in the webview storage;
where storage is refused, they open expanded and still fold for the session.
**Folding hides entries, never the selection**: the settings panel keeps the
effect even when its section is folded. **New effect** sits
outside the sections and stays visible. In the collapsed column a header
shrinks to a rule and its chevron, so a folded section can still be reopened.

### A single "applied" effect, that of the selected device

Marking as active the effects of all devices in a list that describes what
*one* device does is not a simplification, it is false information.
What this session has set is therefore remembered **per device**, not in a
global field.

The word is "appliqué" (applied) and not "actif" (active), since selecting starts the preview:
"actif" covered both states at once, yet they have nothing in common — one says
what the keyboard does, the other what you are looking at.

### Selecting starts the preview, "Appliquer" sends to the keyboard

You used to tune blind and then discover the result on the keyboard. It is
now the reverse: **selecting an effect runs it on screen**, you
adjust while watching, and "Appliquer" (Apply) becomes the action that commits the hardware.

The simulator is in the right-hand panel: list on the left / rendering on the right
here, code on the left / rendering on the right in the editor. Same grammar, and **a single
drawing** — `KeyboardSimulator` is the same component on both sides, there are not
two drawings to keep in agreement. The layout comes from the selected device:
`get_layout` if it is open, `get_default_layout` otherwise.

> ⚠️ **The trap, and it was blocking.** The engine runs one effect per device
> (§4, deliberate). Previewing Y on a keyboard running X would have stopped it:
> in other words, **browsing the gallery would have turned off the current lighting**. It
> would only have shown up once shipped.
>
> The way out is a **separate preview loop**, with no HID output, that borrows
> the *layout* of the selected device without taking anything else from it.
> The architecture already allowed it: `DeviceOut::present` returning `None` means
> "no device open, this is not a failure". Nothing was invented, a
> semantics was put on machinery that already existed.

What the simulator shows is therefore one or the other, and **the screen says which**:
the real keyboard frames when the selected effect is the one running there, and
the preview otherwise. `engine_status` puts the two in two separate fields, and nothing
mixes them along the way — see `docs/api/commands.md`.

What is **stopped** when: the preview stops when leaving the screen and when the
window is hidden — there is no one left to watch —, whereas the applied effect
survives both. That is the whole difference between what the keyboard does
and what you are looking at.

**The cost is bounded on the action side.** Each preview builds a
QuickJS context and destroys one; browsing the gallery with the keyboard arrow keys would
produce several per second. The window therefore waits for the selection to settle
(180 ms). Bounding it in the engine would have forced a choice between making the
last selection wait and losing it, and the window would then have had to reconcile what
it believed it had asked for with what is running.

### Each device states its own status

A dot on each device's pictogram gives its state: filled when controlled and
open, a ring when not open, the warning colour when unplugged. The state is also
in the tooltip and the accessible name, so colour is never its only carrier. A
summary badge in the navigation bar could not say which of two keyboards
dropped; the dot can, and it stays visible when the column is collapsed to
icons.

The editor has no devices column: it shows the target device's dot next to
Apply, the only place there that signals a loss.

The count moves to the window title — "Candeo - 2 devices controlled", with how
many are unreachable when some are. A device adopted but unplugged is counted
**and** said to be unreachable: merging the two would make "controlled but
absent" inexpressible.

### Two deliberate departures from the mockup

| Mockup | Here | Why |
|---|---|---|
| A mouse drawing | The only known layout | The mockup uses it to illustrate that an effect receives *a layout*, not a keyboard. Drawing a mouse that no survey describes would be inventing hardware. |
| A color swatch for hardware effects | Muted dot | The swatch is **sampled by running the effect**. The firmware runs those: the app never sees their frames, and four plausible colors would describe an effect nobody has looked at. |

The third departure — "the preview does not run without a controlled device" — is no
longer one. It was due to the preview being a device's loop; the preview
loop targets none, it borrows a layout. Without a controlled device it
therefore borrows the default layout, and **nothing is written to a device that
the user has not authorized** — this is the adoption invariant, and it is upheld
by construction rather than by abstention.

---

## 9. Settings are a form, not an editor

This is **the piece that serves the audience that will never write code**. Someone who
wants "the wave, but slower" does not need to open Monaco: they need
a slider. The whole rest of the app has served those who write effects;
this column serves the others.

The controls are **generated from the manifest**, never written for a particular
effect. The form knows the four kinds of `ParamSpec`, and nothing
else: an effect installed tomorrow gets its settings without anything changing
here.

| Kind | Control | What goes with it |
|---|---|---|
| `number` | `min`/`max`/`step` slider | the value, with the decimals the step calls for — point and not comma, as in the manifest |
| `color` | color picker | the `#rrggbb` code, spelled out |
| `boolean` | checkbox | "activé" / "désactivé" (enabled / disabled) |
| `choice` | list | the selected option |

A color picker *is* a color — it is the only place in
the app where color is the subject and not an interface role, and it is
the accepted exception to the design-token rule: no color is hard-coded
apart from those the keyboard **emits**. The hex code therefore always
accompanies it: it can be read, noted down and dictated, which a colored dot does not allow
— and no information in this form is carried by color alone.

### Three destinations for one gesture

Moving a slider writes to three places, which have neither the same rate nor the same
lifetime:

| Destination | When | What it is |
|---|---|---|
| window memory | immediate | what the form displays |
| render loop | at most 25 times per second | `set_effect_params`, live |
| `settings.json` | **at the end of the gesture** | `remember_effect_params` |

**The rate towards the engine is bounded, and intermediate states are overwritten.**
A mouse drag produces dozens of events per second; the loop
rereads the parameters on every frame, i.e. thirty times per second. Sending
faster than it reads means replacing a JSON nobody has looked at yet. Only
one send is in flight at a time, and the last requested state always goes out — what
you see on screen is the only one that matters, and it is never lost.

**The disk write goes out at the end of the gesture, not after a pause.** A slider
emits `input` while you drag it and `change` when you release it: the first
feeds the loop, the second writes. A two-second drag therefore produces
**one** write, that of the value you stop at — `settings.json`
is written via a temporary file then a rename, it is a complete operation.

The distinction is not cosmetic. A simple timer — "600 ms without
movement" — would lose the last setting every time you **close the
window** right after: closing destroyed the web view without going through
Vue's hooks, and that is how the app is meant to be used, not an edge case —
an effect keeps running with the window closed. The timer remains, as a safety net
for the cases where `change` does not arrive, backed by a `pagehide`; but neither
of the two is the nominal path, and neither of the two could be.

> **Since issue #46, the close button hides instead of destroying**: the web view
> survives, its timers with it, and `pagehide` therefore no longer fires when
> the window is closed — only when the app exits. The safety net
> loses reach and the timer gains as much; what does not change
> is that the nominal path remains the write at the end of the gesture. The only remaining case
> is a write pending at the moment you choose "Quit Candeo", and
> `change` has almost always already sent it.
>
> A window that survives hidden has a second effect, and it reaches further:
> **its snapshot goes stale**. What it only reads at mount — the
> device list, `settings.json` — can be several days old when it comes back,
> and the icon menu has controlled effects in the meantime. Hence the
> `candeo://etat-change` event, emitted by the icon and when the window is reopened:
> it is the only thing that is pushed rather than polled, and it is
> because polling the disk every second for a few changes per
> session would be the wrong trade-off.

The end of a gesture is not always rare, either: a keyboard arrow key
held down on a slider emits `change` **on every repeat**. Two
writes for the same pair are therefore kept at least 250 ms apart; below that, the
timer takes over and writes the last state on release. Nothing is
lost, it is delayed. A gesture that does not repeat — the click on "rétablir" (reset) — lifts
this gap: it has no reason to wait because a slider has just been
released.

### Why disk, and not just the session

Remembering settings in memory would satisfy the letter of the issue — switching
effect and coming back loses nothing. But the person this form serves sets "the
wave, but slower" **once**; making them set it again at every launch
would amount to shipping a setting that cannot be kept, that is, a
demo. Someone who writes code iterates and has nothing to remember — it is
the other audience that pays for a session-only memory, and it is precisely the one that
this column targets. Adopting a device is persistent for the same reason:
a decision made once is not asked again.

The key is the **device / effect** pair, and only what **differs from the manifest**
is written; the details and the fate of the serial number are in
[`../api/commands.md`](../api/commands.md) §Settings.

### Tuning an effect you have not applied

A parameter only changes something in a running loop. The form
was therefore **inert** outside the applied effect, and it said why — which
amounted to tuning blind and then discovering the result on the keyboard.

**The preview loop removes this trade-off** (§8): there is now a loop to
adjust as soon as an effect is selected, the controls are live, and what you
tune shows on screen without anything going to the keyboard. It is the best
of both: you adjust while watching, and starting a loop on a keyboard remains an action
you decide on.

The other possible answer — applying the effect on the first slider movement — was
rejected, and the preview does not rehabilitate it: starting a loop on a keyboard
is an action you decide on, that is the whole meaning of "Appliquer" and of adoption
before it. Letting a mouse drag do it instead would turn a setting
into a takeover.

The form stays inert in the two cases where there is nothing to adjust: a
**hardware** effect, whose firmware exposes no settings, and the brief moment
before the preview has started.

### Saying it is saved

Settings have been kept since issue #28, and **nothing on screen hinted
at it**: you tune, you close, and you have no reason to believe it held.
That was the real flaw of this persistence, not its mechanism.

The form therefore says so, below the controls, and it says which of the two states it
describes — "values declared by the effect, anything that departs from them is saved" or
"settings saved for *this device*, restored at the next launch". The
devices column does the same for the applied effect: a stopped device
that **remembers** its last effect shows it, rather than "aucun
effet" (no effect).

> **A `fieldset`, and its trap.** The inert state is decided once, on the
> group: `<fieldset disabled>` disables all descendant controls, the
> reset button included, and a field added tomorrow is disabled without
> anyone thinking of it — the same form as the column collapsing in §8.
>
> But a `fieldset` carries an implicit minimum width (`min-width:
> min-content`) that no style reset removes. Without `min-width: 0`, the
> longest label — written by the effect, so arbitrary — imposes its width on the group
> and the column overflows instead of shrinking.

### An effect with no parameter says so

And it does not say so the same way depending on its kind: a host effect "declares
none", a hardware effect "exposes none to the app" — the
firmware runs it, its settings do not go through here. An empty panel
would leave you looking for something that failed to load.

---

## 10. Removing an effect, resetting the configuration to defaults

Two destructive actions arrive together (issues #41 and #42), and **the only
thing that matters is that they cannot be confused.**

| Action | Where | What goes |
|---|---|---|
| Delete an effect | library, third column, one effect at a time | `effects/<id>/`, source included, the settings remembered for it, and its application on devices |
| Reset the configuration to defaults | "Périphériques" screen, at the very bottom | `settings.json`: adoptions, brightness levels, applied effects, effect settings |

### They do not live in the same place, and it is not a matter of tidy arrangement

Deleting an effect removes **hand-written code**, which nothing reinstalls.
Resetting the configuration to defaults touches **no** effect — it is the
data / config distinction that storage has upheld from the start, and this is where
it protects something: if the interface suggested otherwise,
someone who only wanted to un-adopt a keyboard would lose their work.

Hence the placement. The reset is on the screen where you **fill in**
`settings.json` — where you decide what is controlled, ignored, left alone — and
not in the library, where the neighboring action erases code. Deletion is
in the library, on the effect you are looking at, one per effect.

### What is offered, and to what

**Every effect that is a file**, shipped ones included. A hardware effect lives
in the firmware: there is nothing to remove, and the button does not appear for
it, rather than appearing and failing — a button that always fails teaches only
its own uselessness.

### Both ask for confirmation, and the confirmation says what goes

Not a modal dialog: an inline panel in the column, next to what it describes. The
reset confirmation **lists** — devices go back to
`detected`, effects stop, the backlight turns off, settings are
forgotten — and its last line is the most important: *your effects are not
touched*.

The question is tied to an **identifier**, not a flag: the screen is not
frozen while it is being asked, and a boolean would end up confirming the
deletion of an effect other than the one that was designated. Changing the selection
withdraws the question rather than letting it resurface on return.

### Loops stop before the write, and Rust takes care of it

A deleted effect whose loop survived would keep running an
`effect.js` **loaded in memory**: no error, no sign, and a device
controlled by an effect missing from the library. A reset that emptied the
device table while an effect is running would leave loops that no
decision designates anymore.

The stop is therefore done on the Rust side, in the command, not orchestrated from the
window: it is the only place where the invariant holds whoever the caller is.
The exact order of the two commands is in
[`../api/commands.md`](../api/commands.md) §Effect library and §Settings.

The window nevertheless has something to forget: it holds in memory the
settings read at startup and what the session has set as a hardware effect. Without
this forgetting, the first slider movement would rewrite what Rust has just
erased.

### Turning off rather than leaving the last frame

Stopping a loop leaves the keyboard on its last frame, and a frozen frame
looks like an effect still running. The reset therefore sets `Effect::Off`:
the firmware runs it, the cost is nil, and the device ends up
in a state that can be read. A keyboard that refuses to turn off interrupts nothing —
turning off is a nicety, not the action itself.

### What this is not: a place to free resources

`prepare()` creates a QuickJS `Runtime` and `Context` **per loop**, and both
are destroyed with it: the whole JavaScript heap goes with them. There is nothing to
free by hand, and no `dispose()` entry point on the effect side is
desirable — it would put user code on the stop path.
