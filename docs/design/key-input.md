# Key presses for effects

Effects that react to the keys someone presses: a ring spreading from each
pressed key (Ripples), and whatever an author writes next. Status: proposed.

## Why this needs a design

Effects run in Rust, one loop per device, **with the window closed** too. So key
presses cannot come from the window: they have to be read system-wide, on the
Rust side, while someone types in any application. That is exactly what a
keylogger does. The mechanism is only acceptable with rules that hold in the
code, not in intentions — §3.

## 1. What an effect sees

An effect declares that it reads keys, and receives the recent presses with
each frame:

```ts
export default defineEffect({
  inputs: ['keys'],
  render({ layout, time, presses, frame }) {
    for (const { key, at } of presses) {
      const age = time - at   // seconds since this key went down
      …
    }
  },
})
```

- `presses`: the keys pressed recently on this layout, oldest first, each as
  `{ key, at }`. `key` is the layout's own `Key` object, found by scancode (#98);
  `at` is on the same clock as `time`. A press on a key the layout does not have
  is dropped.
- A press is a key **going down**. Auto-repeat while a key is held is not a new
  press, and releases are not reported.
- Only presses younger than **10 seconds**, at most the **last 32**: enough for
  any trail an effect draws, and nothing more is kept (§3).
- An effect that does not declare `inputs: ['keys']` always receives an empty
  list, and nothing is captured on its behalf.

The frame stays a function of `time`, `params` and `presses`, with nothing kept
between frames: a skipped frame, the preview and the device show the same thing.
The swatch is sampled with no presses, so a key-reactive effect should draw
something at rest — Ripples has a dim background color.

**The API version stays 1.** Nothing is released yet, so no effect written for
a published version can meet a Candeo without `presses`. The version check
(`cache_effect` in `docs/api/commands.md`) is for changes made after the first
release.

## 2. Capture on Windows

**Raw Input**, on a thread of its own with a message-only window, registered for
keyboards with `RIDEV_INPUTSINK` so input arrives without focus.

- It receives a **copy** of the input. A low-level hook (`WH_KEYBOARD_LL`) sits
  in the path of every keystroke on the system: a slow hook delays typing
  everywhere, Windows silently removes hooks that take too long, and security
  tools flag them. The keyboard's own HID interface is not an option either:
  Windows opens keyboards exclusively.
- Each `WM_INPUT` gives the make code and its `E0` / `E1` flags, turned into the
  layout's scancode (`0xE01D`, right Ctrl; `0xE11D`, Pause). The extra codes
  Windows synthesizes — the fake Shift around extended keys (`0xE02A`,
  `0xE036`), the `0x45` following Pause's `E1 1D` — are dropped.
- Keys currently held are tracked to tell a press from auto-repeat.
- **Which keyboard.** The device name Raw Input reports carries its VID and PID.
  A device loop receives only the presses of **its** keyboard: typing on a
  laptop's own keyboard does not light ripples on the external one. The preview
  loop receives presses from any keyboard, since it draws a layout, not a
  device. Injected input (no device) reaches the preview only.

**Elsewhere, nothing yet.** Linux means evdev and read access to input devices
(the `input` group), and Wayland offers no global capture: later, and an effect
declaring keys simply receives no presses there.

## 3. Rules the code holds

| Rule | How |
|---|---|
| Capture runs only when needed | registered while at least one running loop — a device's or the preview — runs an effect declaring `inputs: ['keys']`, unregistered (`RIDEV_REMOVE`) as soon as none does |
| Positions, never characters | only scancodes and times are kept; nothing turns a press into a character |
| Nothing written | no scancode in the log at any level, nor in the copied diagnostic; the log says only when capture starts and stops. An effect reading keys chooses its error text and could put presses in it: that text stays out of both, and only the window shows it (#44) |
| Nothing kept | the last 32 presses younger than 10 seconds, in memory; older ones are dropped |
| Visible | the gallery marks an effect that declares keys ("réagit aux frappes") |

## 4. Engine

- `runtime::presses` owns the capture thread and a ring of presses (instant,
  scancode, VID/PID or none). Loops that need keys hold a guard; the first guard
  registers, the last one dropped unregisters.
- Each frame, a loop running a key-reactive effect takes the presses newer than
  its start (and from its keyboard, for a device loop), resolves each scancode to
  the key's position in `layout.keys` through a table built once, and passes
  `[{ "k": <position>, "at": <seconds> }]` to `__candeo_render`, which hands the
  effect the `Key` objects. Other loops pass nothing and pay nothing.
- The capture sits behind a channel, so the engine tests inject presses without
  a keyboard.

## 5. Ripples

A shipped effect, `packages/effects/Ripples.ts`: from the center of each pressed
key, a ring grows at a set speed in physical distance and fades over its
lifetime, over a dim background color. Settings: ring color, background, speed,
ring width, lifetime. Descriptions and labels in English and French, as the
other shipped effects.

**In the preview**, nothing specific: the preview loop receives presses like a
device loop, so the simulator shows the ripples while someone types — in Candeo's
window or anywhere else. The simulator does not highlight pressed keys for other
effects: that would mean capturing for every effect, and the simulator draws the
engine's frames, nothing else.

## 6. Out of scope

- Capture on Linux and macOS.
- Choosing keys by character in an effect (character → scancode through the
  system layout): only if an effect needs it.
- Mouse input, key releases, held-key durations.

## Order of work

1. `inputs` and `presses` in the bootstrap and the TypeScript API.
2. `runtime::presses`: Raw Input capture, the ring, guards, the device filter;
   engine tests with injected presses.
3. Ripples, and the gallery mark.
4. In the app: typing in another application lights ripples on the keyboard and
   in the preview; capture registered only while Ripples runs.
