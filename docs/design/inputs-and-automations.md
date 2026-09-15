# Inputs and automations

Lighting that reacts to more than key presses — the time, the music playing,
signals sent by other software — and rules that interrupt the configured effect
for a while. Status: proposed. Each part lands in its own pull request, in the
order at the end.

## Why this needs a design

Two different things are asked, and they must not be confused:

- **Inputs** feed an effect while it runs: a spectrum drawn from the music, the
  hour drawn on the number row. The effect stays the one applied; it only sees
  more than `time`.
- **Automations** change *which* effect runs: at every full hour, show the clock
  for ten seconds; when the doorbell rings, flash the keyboard. They interrupt
  the configured effect and give it back.

Both would otherwise grow one feature at a time, each with its own capture, its
own settings and its own idea of what the device is doing. This document fixes
the shared shapes first.

## 1. Inputs: the shape key presses already set

`docs/design/key-input.md` settled the pattern, and every input reuses it:

| Rule | For every input |
|---|---|
| Declared | an effect lists what it reads: `inputs: ['keys', 'clock', 'audio']` |
| Captured only when needed | a source runs while at least one loop — a device's or the preview — runs an effect declaring it; the last guard dropped stops it |
| Compact, per frame | the effect receives a small snapshot with each frame, next to `time`, never a stream it has to buffer |
| Same in the preview | the preview loop receives the same snapshot, so the simulator shows what the keyboard shows |
| Nothing kept, nothing written | only what the next frame needs stays in memory; values never reach the log or the diagnostic, only when a source starts and stops |
| Visible | the gallery marks an effect by what it reads ("reacts to key presses", "reacts to sound"…) |
| Testable without hardware | each source sits behind a channel, so engine tests inject values |

An effect that does not declare an input receives an empty or neutral value, and
nothing is captured on its behalf. The swatch is sampled with neutral inputs, so
a reactive effect draws something at rest.

The effect API version stays 1 until the first release (`key-input.md` §1).

## 2. Sources

### 2.1 Clock

```ts
render({ clock }) // { year, month, day, weekday, hours, minutes, seconds, ms }
```

The local wall-clock time, read once per frame. `time` stays the seconds since
the effect started; `clock` is what a clock face needs. Nothing to capture and
nothing private, so no guard: every effect declaring `clock` gets it.

**Showing the hour** takes two pieces:

- **The effect** draws the time — the current hour and minutes lit on the number
  row, a binary clock on the function keys. It is an ordinary effect with
  `inputs: ['clock']`: applied by hand, it shows the time all along. A shipped
  *Clock* effect comes with this part.
- **When and for how long** is an **automation** (§3): *every 3600 seconds, for
  10 seconds* shows the clock on the hour and gives the keyboard back; *every
  second, for 1 second* never gives it back, which is the continuous display
  again, on top of whatever is applied.

Later, if asked: sunrise and sunset, which need a location.

### 2.2 Sound playing

```ts
render({ audio }) // { level, peak, bands: number[16], beat }
```

- **Source**: what the computer plays, not the microphone.
  - On Windows, WASAPI loopback on the default output device, through `cpal`
    (or the `wasapi` crate if loopback turns out incomplete there).
  - On Linux, the monitor source of the default sink, through PipeWire or
    PulseAudio.
- **Analysis in Rust, once per frame**, whatever the number of effects reading it:
  - an FFT (`rustfft`) over the latest window of about 40 ms;
  - `level` and `peak` in 0..1;
  - 16 logarithmically spaced `bands`, normalized in 0..1 with a short decay so
    bars do not flicker;
  - `beat`, true on the frame an onset is detected (spectral flux over a moving
    threshold).
- **Privacy**: samples never leave the analysis; only these numbers reach the
  effect. Nothing is recorded, and the log says only when capture starts and
  stops.
- **Silence** is `level: 0` and flat bands, the same as no capture: an effect
  cannot tell "nothing plays" from "capture unavailable", and does not need to.
  The gallery says when capture failed.
- **Shipped effects**: a spectrum across the columns, and a pulse on the beat.

The microphone is the same pipeline on an input device; it comes only if an
effect needs it (a "microphone muted" light is better served by signals, §2.3).

### 2.3 External signals

Named values that other software sends to Candeo:

```text
POST http://127.0.0.1:<port>/signals   { "doorbell": "ring", "ci": "failed", "volume": 0.4 }
```

```ts
render({ signals }) // { doorbell: 'ring', ci: 'failed', volume: 0.4 }
```

- **Why it matters most**: once other software can push values, every
  integration Candeo does not write becomes a script — Home Assistant, a CI job,
  a mute state, a recording state.
- **The local API**:
  - **Off by default**, enabled in Settings.
  - Listens on loopback only.
  - Every request carries a token, shown in Settings with a button to generate
    a new one.
  - Listening on the local network is a second, explicit choice, for Home
    Assistant on another machine.
  - Values are strings, numbers or booleans; a signal expires after a lifetime
    its sender can set (default 60 s), so a crashed sender does not leave the
    keyboard red forever.
- **Also from the command line**: `candeo signal ci=failed`, which talks to the
  running instance through the single-instance channel, for scripts that would
  rather not handle HTTP.
- **Home Assistant**: the HTTP API is enough for its `rest_command`. MQTT, with
  Candeo as a client of the broker Home Assistant already runs, needs no inbound
  port and could expose Candeo as an entity. It is a later step, if HTTP proves
  awkward there.
- Signals are also **automation triggers** (§3): "when `doorbell` becomes `ring`".

### 2.4 Later, if asked

| Source | What an effect would see | Why later |
|---|---|---|
| System | CPU load per core, memory, network rates (`sysinfo`, once a second) | easy; temperatures and GPU need WMI or NVML, sometimes rights |
| Screen colors | average colors along the screen's edges | screen capture (Windows Graphics Capture, a PipeWire portal): costly, and the most sensitive input there is |
| Games | health, ammunition, round state | per-game integrations (CS2 and Dota "Game State Integration" post JSON locally): signals (§2.3) can carry them first |

## 3. Automations

### 3.1 The principle: one effect per device, interrupted

A device still runs **exactly one effect** at a time. What changes is where that
effect comes from:

- the **applied effect** — the one chosen in the gallery, remembered in
  `activeEffects` — is the device's resting state;
- an **interruption** replaces it for a while, then gives it back.

At any moment the engine runs the interruption with the highest priority that is
active for the device, or else the applied effect. An interruption never
rewrites `activeEffects`: when it ends, the device goes back to what someone
chose, with its settings.

### 3.2 Rules

A rule is one sentence someone can read back:

> **Every** *3600 seconds* · **on** *DeathStalker V2 Pro* · **show** *Clock* ·
> **for** *10 seconds*

```json
"rules": [{
  "id": "…", "name": "Hourly clock", "enabled": true,
  "devices": [{ "vid": 5426, "pid": 658 }],
  "when": { "kind": "schedule", "every": 3600, "aligned": true },
  "show": { "effect": "Clock", "params": {} },
  "for": { "seconds": 10 }
}]
```

- **The schedule**: one trigger, in seconds, with a few options:

  | Option | Meaning | Default |
  |---|---|---|
  | `every` | seconds between two occurrences, 1 or more; the interface offers 1 s, 1 min, 15 min, 1 h | — |
  | `for` | seconds each occurrence lasts | 10 |
  | `aligned` | occurrences fall on the clock (on the minute, on the hour) rather than counted from when the rule was enabled | on when `every` divides a day |
  | `between` | only between two times, `22:00`–`07:00`; outside them the rule sleeps | always |

  - **When `for` reaches `every`**, the next occurrence starts before the current
    one ends: the interruption simply continues. The effect is not restarted, so
    *every 1 s for 1 s* is a steady display, not a flicker.
  - **`between` covers time windows**: *every 1 s for 1 s between 22:00 and
    07:00, show Off* turns the keyboard off at night. No separate trigger is
    needed.

- **Other triggers, with their own pull requests**:

  | Trigger | Examples |
  |---|---|
  | `signal` | `doorbell` becomes `ring`; `ci` equals `failed` while it does |
  | `idle` | no key pressed for 10 minutes (reuses key capture) |
  | `app` | an application in the foreground (later) |

- **Duration for these triggers**:
  - `for: { seconds }` ends the interruption after a time (a flash);
  - `while` lasts as long as the trigger holds (a signal that keeps its value,
    idleness).
- **Action**: any effect of the library or a hardware effect, including *Off*,
  with its own settings. A rule does not borrow the device's saved settings for
  that effect: the hourly clock and the clock applied by hand need not look the
  same.
- **Priority**: the order of the list. When two rules are active on one device,
  the higher one runs; when it ends, the next active one, or the applied effect.

### 3.3 What someone does during an interruption

- **Applying an effect by hand** makes it the new applied effect, and ends the
  current interruption: a gesture always wins over a rule. The rule triggers
  again at its next occurrence.
- **Resume** on the device card and in the tray ends the interruption now.
- **Pause automations**, in the tray and in Settings, suspends every rule until
  it is turned back on: a "do not disturb" for a meeting or a game.

### 3.4 Engine (Rust)

Rules must work with the window closed, so they live in Rust, next to the
engine:

- A **resolver**, a pure function, decides for each device what should run and
  why: `(now, rules, signals, idle times, applied effects, paused) → { effect,
  params, reason, until }`. It holds all the logic above and is tested without a
  clock or a device.
- A **scheduler** thread calls it on each event that can change the answer — a
  signal received, a rule edited, a device opened — and on a one-second tick for
  schedules and durations. When the answer changes for a device, it starts the
  new effect on that device through the same path as `start_effect`, without
  writing `activeEffects`.
- Going back restarts the applied effect: its `time` starts again from zero.
  Keeping the interrupted loop alive in the background would mean two loops per
  device, which §3.1 excludes.
- `engine_status` gains `interruption: { rule, until } | null` per device, and
  the state-changed event tells the window and the tray.
- Rules are written in `settings.json` under `rules`, validated when read. A rule
  naming a deleted effect stays, marked broken, and does nothing.

### 3.5 Interface

- **An Automations tab** in the rail, between Effects and Devices:
  - Each rule is a sentence of chips — *Every* / *on* / *show* / *for*, and
    *between* when set — each opening a small picker; the durations take presets
    and any number of seconds. The effect picker is the gallery's list; its
    settings form is the gallery's.
  - A switch per rule, the order changed by dragging, and a **Try** button that
    triggers a rule once, now.
  - Empty state: examples to start from (hourly clock, night off, and a doorbell
    flash once signals exist), each creating a disabled rule to adjust.
- **The device card** in the gallery shows an interruption where it shows the
  running effect: "Clock — for 7 s, then Ripples", with **Resume**.
- **The tray**: the device submenu names the interruption the same way, and a
  **Pause automations** check item sits above *Open window*.
- **The simulator** shows what the device shows, interruption included, since it
  draws the device loop's frames.

## 4. Order of work

Each step is one pull request, with its issue:

1. **Resume the applied effect** when a device opens — at startup, on adoption,
   on replug (#81) — behind a Settings option, on by default (#102).
2. **Clock input** and the shipped *Clock* effect (#105).
3. **Automations with the `schedule` trigger**: the resolver, the scheduler,
   `rules` in `settings.json`, the Automations tab, interruptions on the device
   card and in the tray. The periodic clock and the night window come with it
   (#106).
4. **Sound input** and two shipped effects (#107).
5. **External signals**: the local API, the command line, `signal` triggers,
   the Home Assistant example (#108).
6. **`idle` trigger**, then system metrics, if asked.

## 5. Out of scope

- Several effects composed on one device (layers, masks).
- Screen colors and game integrations beyond what signals carry.
- macOS.
