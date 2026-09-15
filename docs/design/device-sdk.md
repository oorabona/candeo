# Device SDK — contributing a device only you own

This document prepares [#34]. It does not describe what exists: it settles the
**vocabulary** through which a contributed device will state what it can do, and
the rules that make a contribution **reviewable by someone who does not own the
hardware**.

It is written for one specific person: the one who has a device we do not have,
who wants to make it work, and who will have to convince a reviewer unable to
verify anything by themselves.

> **What is not decided here.** The file format of a layout, the bytes of any
> protocol other than the DeathStalker's, and the implementation. The code blocks
> below show the **shape** of a declaration, not a settled syntax.

---

## 1. Three things, two of which are already data

A device boils down to an **identity** (VID/PID, interface of the composite
device), a **layout** (addressable positions, names, geometry) and a way to
**build its reports**. That is the analysis in [#34], and it holds.

What this document adds is that a fourth one is missing, absent and invisible
today: **what the device can do**. `Layout` carries `name`, `vid`,
`pid`, `interface`, `rows`, `cols`, `matrix`, `keys` — enough to open the right
device and push a frame to it, nothing that allows answering "does this
effect make sense here". As long as there is only one device, the question does
not arise: the answer is yes, always. It arises with the second one.

### Why a capability and not a model

The temptation is to let an effect name the devices it targets. That is the
wrong axis, and [#44] §5 gives the definitive reason:

> A list of models is a **closed world**. It can know nothing about hardware
> released after it, and its author cannot try it anyway. A **capability**
> requirement is open: it can be matched against any layout, including those
> that do not exist yet.

The consequence applies to the SDK as much as to effects. **Contributing a
device means declaring what it can do** — after which an effect written today
works on a device added two years from now without anyone reopening its list, and
a device with a peculiarity can expose it without an effect having to hard-code
a model.

⚠️ **This vocabulary is a public interface.** An exported effect contains it
spelled out; changing it after the fact breaks files that are no longer in our
hands. That is the reason for the three admission criteria in §3.

---

## 2. The kind of device

The layout carries a **kind** — `keyboard`, `mouse`, `mousepad`, … — and every
effect **must declare** the kinds it targets. The obligation was settled
in [#44] §5 and is not reopened here:

> An optional field produces silence, not an answer: nobody fills it in, and
> "absent" ends up meaning "I did not think about it" as much as "it works
> everywhere".

### Why a kind, when there are capabilities

Because the two answer different questions, and neither replaces the other:

| | Question | Who decides |
|---|---|---|
| **Capability** | Does it **work** here? | the mechanics |
| **Kind** | Does it **make sense** here? | the effect's author |

"Balayage" (Sweep) would work perfectly well on a mouse pad fitted with a grid:
no capability refuses it. Its author may nonetheless know that its trail
assumes rows of keys and does not look like much elsewhere. The kind is the only
place where they can say so.

It also serves what the capability vocabulary must **above all not** have to
express: what the object looks like. The simulator draws a keyboard because it
is one ([`studio.md`](studio.md) §4); there is no "looks like a keyboard"
capability, and one should not try to write it.

### The list is closed, and it is not a closed world

Adding a kind takes one line in a list in the repository, hence a PR — the same
one that brings the device. This is deliberate: `mouse` and `souris` and
`pointing-device` written freely by three contributors would give three kinds
that no effect can target together.

> **This is not the closure that [#44] condemns.** A list of *models* goes stale
> because hardware changes; a list of *kinds* does not go stale because
> categories of objects shift one notch per decade. A mouse released tomorrow
> declares itself `mouse` and every "mouse" effect reaches it without a line
> changing. That is exactly the opposite of a VID/PID list.

### "All kinds" is written out

```ts
kinds: 'all',          // un choix, pas un oubli
kinds: ['keyboard'],
kinds: ['mouse', 'mousepad'],
```

---

## 3. The capability vocabulary

### 3.1 Three admission criteria

A term enters the vocabulary only if it passes all three. This is what prevents
the two drifts — a vocabulary too fine-grained, impossible to fill in, and a
vocabulary too coarse, that nobody can rely on.

1. **An effect depends on it.** There exists, or one can name precisely, an
   effect that changes behavior — runs or refuses — depending on the term's
   value. Without that the term is filler: it costs every contributor a decision
   and serves nobody.
2. **A contributor can answer from their survey alone.** Without owning another
   device, without reading a third-party driver's code, without guessing. A term
   that cannot be answered is a term that will be answered **wrong**.
3. **Its absence has a safe meaning.** A layout written before the term was added
   must stay correct. Without this rule the vocabulary can no longer grow, and
   half of its value disappears.

### 3.2 The six terms

| Term | What it says | The effect that depends on it |
|---|---|---|
| `directFrame` | the host can set a frame | **all of them** |
| `color` | `rgb` or `mono` | anything that deals with hue |
| `matrix` | positions form a grid, neighborhood has a meaning | "Balayage" (Sweep), "Onde diagonale" (Diagonal wave) |
| `geometry` | every position has a physical place | "Onde radiale" (Radial wave) |
| `namedKeys` | every position carries the name of its key | highlighting a shortcut |
| `zones` | named parts outside the grid | a wheel pulse |

---

**`directFrame` — the host can set a frame.**

It is the only term everything depends on, and the only one that might seem
useless since every effect requires it. It earns its place through the opposite
case: a device whose firmware only exposes its own effects exists, and deserves
a layout — we want to name it, list it, set its hardware breathing effect. What
we do not want is to start an effect on it, animate the simulator, and leave the
user staring at a dark device. That is precisely the silence that cost a whole
stretch of debugging ([`effects-runtime.md`](effects-runtime.md) §7); it must
not come back in through a contributed device.

**Filled in by**: the contributor found the host-controlled mode, or did not
find it. On the DeathStalker it is effect `0x08` ([`deathstalker-v2-pro.md`](../protocol/deathstalker-v2-pro.md) §4).

---

**`color` — `rgb` or `mono`.**

The only term in the set that cannot be read anywhere in the data: a monochrome
layout has the same matrix, the same names, the same geometry. It therefore has
to be declared, and it is the only one.

A white or single-color backlight is nothing exotic, and an effect whose subject
**is** hue — "Onde radiale", which rotates `hsv` — does not produce a degraded
result there: it produces a surface of roughly constant brightness, that is,
nothing. Better for the gallery to say so.

⚠️ **Absent means `rgb`**, because that is what all existing layouts are. The
general rule is in §3.4; this term illustrates it.

---

**`matrix` — positions form a grid.**

The term with the heaviest consequences, because it is **already assumed
everywhere and declared nowhere**. The three shipped effects read `key.row`:

```js
const k = Math.max(0, 1 - Math.abs(key.row - head) / trail)   // balayage.js
const steps = key.col + key.row                               // onde-matricielle.js
```

On a device without a grid, `key.row` would be `undefined`, the subtraction
`NaN`, and the color would be clamped to zero: **the three shipped effects
would display black, without a single error.** This demonstrates that the
requirement must be written, and that it must be written **now**, while the
effects concerned are all in our hands.

A quantified requirement is allowed here — "at least three rows", "at least
ten columns" for an effect that draws a gauge — under a strict rule:

> **A threshold may only apply to a number the layout already declares for
> another reason.** `rows` and `cols` have been in `Layout` from the start.
> Allowing thresholds on anything else would sneak in quantities that nobody
> knows how to measure.

---

**`geometry` — every position has a physical place.**

What the device **does not declare**: it only exposes its logical grid. The
rectangle of each key is a hand transcription of the ISO layout.

The effect that depends on it has **existed** since [#60]: "Onde radiale"
computes its distance on the rectangles, and "Onde diagonale" counts steps
through the matrix, `column + row` from a corner. In the grid, the space bar
takes one cell for 6.25 u and gaps count as distance: only the first one needs
the drawing.

What the term would add, and what is still missing: "Onde radiale" **throws** on
an undrawn layout, instead of being refused before being offered. See §4.

**Filled in by**: the contributor drew their layout, or not. It is real and
optional work; not doing it still makes a valid contribution, which only loses
the spatial effects.

It is also what the **simulator** needs to look like the device. A layout
without geometry is not undrawable, it is drawn as a grid of squares — which
[`studio.md`](studio.md) §4 had ruled out for the device we have, and which
becomes the lesser evil again for a device we do not have.

---

**`namedKeys` — every position carries a name.**

The DeathStalker's index → key mapping required cross-referencing the matrix
with the ordered list the device declares (§6 of the survey). It is neither
automatic nor given, and a contributor can legitimately stop before that.

The effect that depends on it is one that lights a **key by its name** rather
than by its position: highlighting the keys of a shortcut, lighting `WASD`,
making only the space bar breathe. Such effects cannot be written portably
today, and the term is what makes them possible without hard-coding a model.

⚠️ **A name is not an identity.** The surveyed DeathStalker is `fr_FR` ISO:
its names are `A`, `Z`, `ù`. An effect looking for `W` on an AZERTY will find it
where a QWERTY puts it elsewhere. The term says "there are names", it promises
no locale layout; what the locale layout carries belongs to provenance (§5).

---

**`zones` — named parts outside the grid.**

This is the term that opens the SDK to devices that are not keyboards, and the
one that requires the most care, because a free-form name is usable by nobody.

> **Roles come from a closed list** — `main`, `logo`, `wheel`, `strip`,
> `underglow`, `wrist` — extended by PR, like kinds. An effect that requires
> `wheel` needs `wheel` to mean the same thing everywhere; if the field is
> free-form, `molette`, `wheel` and `scroll` coexist and the effect never finds
> anything. The displayed label, on the other hand, is free text.

This is where the open-world argument shows best. A wheel pulse written today
for a mouse will work on a 2028 **keyboard** fitted with a lit wheel, without a
line of the effect changing — because it never talked about mice, only about
wheels.

⚠️ **One boolean per component is the drift to avoid.** `hasWheel`, `hasLogo`,
`hasUnderglow`… is an infinite vocabulary that nobody can fill in or keep up to
date. A zone is an **addressable position that carries a role**: nothing more,
and that is what makes it describable.

### 3.3 What stays out of the vocabulary, and why

Ruling something out is also a decision, and it is the one that keeps the
vocabulary small.

| Ruled out | Reason |
|---|---|
| **partial row writes** | true on the DeathStalker, and **invisible to an effect**: an effect writes a frame, it never addresses the bus. It is a transport property, not a capability. |
| **hardware brightness** | the host and the interface depend on it, no effect does: a frame already carries its colors. |
| **firmware effects** | enumerating `Static`, `Breathing`, `Wave`… per model would recreate a closed world of another sort. It is the interface's business, not the effects'. |
| **sustainable frame rate** | it is a **measurement**, not a capability — see below. |
| **LED count** | derived, and above all a trap: 132 positions for 106 keys, and confusing the two freezes a row. No effect must branch on it. |
| **VID / PID** | the very thing [#44] §5 refuses. |

**The frame rate case deserves its own paragraph.** A full update of the
DeathStalker costs 13.1 ms, a ceiling of ~76 frames per second, and that is
what brought the engine down from 60 to 30 (§5 of the survey). A slower
contributed device will exist. So the layout does need to carry that figure — but
as a **dated measurement**, next to the provenance, not as a capability: the host
uses it to tune its loop and to warn, and no effect has to care about it. An
effect that required "at least 30 frames per second" would not be refused
usefully; it would be refused stupidly, whereas slowed down it is still an effect.

### 3.4 How the vocabulary grows without breaking anything

Two rules, and they do not have the same price.

**On the layout side — an added term must have a safe meaning when absent.**
`namedKeys` absent means "no names": safe. `color` absent means
`rgb`, because that is what all layouts written before the term are —
the safe default is not "nothing", it is **what the existing ones already
assumed**. A term without a safe default cannot be added after the fact, only at
a breaking version. This is the constraint that must be checked **before** adding
a word, not discovered afterwards.

**On the effect side — an unknown term is refused with a sentence.** An effect
from a newer version may require a term the host does not know. It is
refused, and it is refused by a sentence that names the term, like `apiVersion` in
[#44] §4 — never by an engine error that nobody connects to their code.

### 3.5 The layout fills in almost nothing: capabilities are **read**

This is the point that holds everything else together, and it is the answer to
"a vocabulary too fine-grained becomes impossible to fill in".

**Only what cannot be deduced is declared.** A contributor does not fill in a
capability questionnaire: they provide the data they surveyed, and the
vocabulary is the reading of it.

| Term | Where it comes from |
|---|---|
| `directFrame` | the driver implements "set a frame", or not |
| `matrix` | the layout carries a grid |
| `geometry` | positions carry a rectangle |
| `namedKeys` | all positions carry a non-empty name |
| `zones` | the layout carries zones |
| `color` | **declared** — nothing in the data reveals it |

A contributor therefore cannot get five terms out of six wrong: they can only
offer fewer, by providing less. And the incentive points the right way — drawing
one's geometry, naming one's keys, opens the contribution to more effects.
Nobody has to decide on a checkbox whose stakes they do not understand.

What remains declared by hand is therefore tiny: the kind, the color depth, and
the provenance of §5. Three things that can be answered without guessing
anything.

---

## 4. Two declarations, matched against each other

### The DeathStalker V2 Pro

What the existing layout would become. The data is that of
[`layout.rs`](../../crates/candeo-device/src/layout.rs), unchanged; everything
new fits in the last two blocks.

```rust
pub static DEATHSTALKER_V2_PRO: Layout = Layout {
    name: "Razer DeathStalker V2 Pro",
    kind: DeviceKind::Keyboard,
    vid: 0x1532,
    pid: 0x0292,
    interface: 3,                       // MI_03: lighting, and lighting only

    grid: Some(Grid { rows: 6, cols: 22, matrix: &[/* 132 positions */] }),
    keys: &[/* 106 touches, nommées et dessinées */],
    zones: &[],                         // tout est dans la grille

    color: Color::Rgb,                  // le seul terme déclaré

    provenance: Provenance {
        firmware: Firmware { major: 1, minor: 5 },
        firmware_read_by: "classe 0x00, commande 0x81",
        surveyed_on: "2026-09-12",
        method: Method::CapturedAndVerified,
        variant: "French (ISO) — fr_FR, relevé par 0x00/0x86",
        sustained_rate: Some(Rate { frames_per_second: 76, measured_on: "2026-09-12" }),
        origin: Origin {
            grid: Source::Surveyed,     // relevé sur l'appareil
            keys_named: Source::Surveyed,
            keys_drawn: Source::Convention,   // dessiné à la main, jamais vérifié
        },
    },
};
```

The capabilities that follow from it, without a line stating them: `directFrame`,
`matrix { rows: 6, cols: 22 }`, `geometry`, `namedKeys`, `color: rgb`, no
zones.

### A three-zone mouse

> ⚠️ **This is not a survey.** No unit has been opened, no frame
> captured. It is an illustration of the **shape** of a declaration for a
> device as far as possible from the only one we have. The bytes, the
> identifiers and the frame rate of a real device cannot be guessed.

```rust
kind: DeviceKind::Mouse,

grid: None,                             // il n'y a pas de voisinage ici
keys: &[],
zones: &[
    Zone { role: Role::Wheel,  label: "Molette" },
    Zone { role: Role::Logo,   label: "Logo" },
    Zone { role: Role::Strip,  label: "Bandeau latéral" },
],

color: Color::Rgb,
```

A frame there is **three** colors instead of 132. Nothing else changes: the
engine still pushes a flat array of `Rgb` into `DeviceOut::present`, and the
trait does not move by a single line. That is the best argument for this
shape — **it adds no data path, only a way of knowing who we are talking
to.**

### What it looks like on screen

| Effect | Requires | Keyboard | 3-zone mouse |
|---|---|---|---|
| "Respiration" (Breathing) | `kinds: 'all'`, nothing | ✅ | ✅ |
| "Balayage" | `matrix { rowsMin: 3 }` | ✅ | ❌ no grid |
| "Onde diagonale" | `matrix` | ✅ | ❌ no grid |
| "Onde radiale" | `geometry` | ✅ | ❌ nothing is drawn |
| Highlight a shortcut | `namedKeys` | ✅ | ❌ nothing is named |
| Wheel pulse | `zones: ['wheel']` | ❌ no wheel | ✅ |

The last row is the one that matters: the day a keyboard with a lit wheel
arrives, its cell turns ✅ **without the effect being reopened**.

### On the effect side

```ts
export default defineEffect({
  kinds: ['keyboard'],
  requires: { matrix: { rowsMin: 3 } },
  render({ layout, time, frame, params }) { … },
})
```

**`requires` is mandatory, like `kinds`.** [#44] only required the declaration
for the kind; this document extends it, and for exactly the same reason — the
three shipped effects read `key.row` without saying so, and would display black on
a device without a grid. An implicit default would not be safer: it would
only be invisible. The migration costs one line per effect, and that line writes
down a truth that was until now assumed.

Requiring nothing is written out, like "all kinds":

```ts
  requires: 'none',
```

### A requirement enforced by the host, not by courtesy

Nothing prevents an effect from declaring `requires: 'none'` and reading `key.row`
anyway. It is the same situation as the out-of-bounds writes in [#44] §4, and it
calls for the same answer: **it is the host that stops it, not the author's
courtesy.**

The lead: the engine builds the `layout` object handed to QuickJS **according to
what the effect declared it requires**. Reading a field that was not required then
becomes a named frame error — "this effect reads `row` without requiring
`matrix`" — which takes the existing error path, the one that thirty consecutive
frames turn into a clean stop. The cost is a few thousand intercepted accesses
per second at 30 frames per second, which is nothing.

Not settled here: this is an implementation lead, not an interface decision.
What is settled is that **the gap between what is declared and what is read
must not be silent**.

---

## 5. Provenance: what the layout was established against

A contributed layout **must** state which firmware it was established on, and
when. Without that, strange behavior at a third party's is impossible to debug —
and the contributor will no longer have their hardware at hand to settle it. This
is what [#35] asks for, and it is the only thing this SDK requires that does not
serve to make the device work: it serves to understand later.

### What is tricky, and why the field alone is not enough

The `release_number` from HID enumeration is `0x0200` across the whole composite
device while the firmware reports itself as v1.5. It is `bcdDevice`, **a frozen
hardware revision**: `hidapi` does not give what we are looking for, and the only
path to the real version is a device command — `0x00`/`0x81` here.

A contributor in a hurry will copy `release_number`, because it is there, it
looks like a version, and nothing contradicts it. Hence the second field:

> **`firmware_read_by` is as mandatory as `firmware`.** "v1.5, read by
> class 0x00 command 0x81" is verifiable and open to discussion. "2.00", alone,
> is not — and it is precisely the value one gets by getting it wrong.

### The fields, and what each one saves

| Field | What it allows, the day something goes wrong |
|---|---|
| `firmware` + `firmware_read_by` | telling "the code is wrong" apart from "the version changed" |
| `surveyed_on` | dating the survey against the manufacturer's update log |
| `method` | knowing whether the bytes were **seen on the bus**, **written and read back**, or **inferred** from a sibling device |
| `variant` | the locale layout: the matrices of an ISO and an ANSI do not carry the same keys |
| `sustained_rate` | tuning the loop on a measurement rather than on the device we have |
| `origin` | knowing, line by line, what was **surveyed** and what was **drawn** |

### `origin` is the most useful field in review

`layout.rs` already says, in prose, that the names come from the survey and the
geometry from a convention: "the two do not have the same status". Today it is only a comment. Carried as data, it tells a
reviewer **what they can challenge and what they must take on trust**, and tells
the interface what it must display next to a device whose drawing nobody here
has verified.

Three values are enough:

- `surveyed` — seen on the device, reproducible by the method in §9 of the survey;
- `convention` — built by hand, accurate in the sense of a common practice; only
  the eye can disprove it;
- `inferred` — copied from a sibling device, **never tried**.

`inferred` is the most important of the three, because it is the one you get
when contributing the second model of a product line without owning it. It must
exist so that it is written down rather than hidden.

### What provenance is not

A declaration, like an effect's `author` ([#44] §1): nothing authenticates it.
The difference is that it is **refutable** — a version, a date and a method can
be matched against a later observation, whereas a name can be matched against
nothing. That is what justifies requiring it without claiming to verify it.

On connection, a different version **warns without blocking** ([#35]): blocking
would make the application useless after a routine update, when the protocol
will very probably not have moved. The warning turns a silent failure into a
stated suspicion — and that is all one can honestly do with it.

> **Already in place for the existing layout**, without waiting for this SDK:
> `Layout::surveyed_firmware` carries the version as **two numbers**
> (`Firmware { major: 1, minor: 5 }`) and not as a string: the survey once wrote
> "v1.5" in some places and "1.05" in others for the same bytes `01 05`, and
> comparing strings would turn a difference in wording into a difference in
> version. It now writes v1.5 everywhere (#74). The inspection on open is in
> [`inspection.rs`](../../crates/candeo-device/src/inspection.rs).

---

## 6. "Driver mode": the step not to copy

This is the warning that on its own justifies an SDK existing, because it is the
step a contributor would take over from an existing driver **without seeing what
it costs**, and that no review would catch if it were not named.

**The fact**: on the DeathStalker, `0x00`/`0x84` returns `0x00` — the device is in
**normal mode**, and our custom lighting works perfectly that way.
OpenRazer, on the other hand, switches its devices into **driver mode** (`0x00`/`0x04` → `0x03`)
when its daemon initializes.

**Why it makes sense for them**: OpenRazer is a **full driver** — it
handles macro keys, DPI, profiles. It needs the firmware to hand
over control.

**What it costs**: in driver mode, the firmware **stops handling
certain keys itself** and merely emits HID events that the host is
supposed to pick up. If nobody listens, those keys no longer do anything —
observed on a Basilisk V3 whose DPI cycle and wheel lock became
inert, and fixed by switching back to normal mode.

> **Candeo only controls the lighting.** Switching brings us nothing and breaks
> keys the device handles very well on its own. **Never write `0x00`/`0x04`.**

### What the SDK does with it — a rule, not just a warning

A paragraph in documentation gets copied less reliably than a line of code.
The warning must therefore be **carried by the shape of the SDK**:

> **The SDK does not expose "send this report". It exposes intents**:
> set a frame, set the brightness, pick a firmware effect.

There is no intent named "change the device mode". A contributor who copies an
initialization sequence found elsewhere therefore has nowhere to put it: they
would have to **step outside the SDK** to do it, which shows up in review instead
of blending into a string of bytes.

And the asymmetry that goes with it:

| | Allowed |
|---|---|
| **Read** | broad — the version, the serial number, the current mode, the descriptor. Reading breaks nothing, and it is what feeds the provenance of §5. |
| **Write** | only lighting intents, only on the interface the layout declares. |

This also answers the open question in [#34] — "a third-party layout must not be
able to write anything to any interface": **the contributed driver opens
nothing**. The host enumerates, filters on `interface_number`, opens, and
hands it a channel already bound to that interface. A driver that never sees
`HidApi` cannot pick the wrong device, nor discover another one.

---

## 7. Shipping a device as data rather than code

[#34] asked the question; the vocabulary above almost answers it on its own. A
layout is already pure data, and capabilities are read from it (§3.5). What
remains in code is building the reports.

**The right unit is not the device, it is the protocol family.** The
DeathStalker is nothing special: it speaks the Razer protocol — a 90-byte
feature report, class `0x0f`, XOR checksum over bytes
2 to 87, one command per row. A second Razer keyboard would require **not a
single line of code**: a family, and data.

What makes a family parameterizable is exactly what varies from one model to
another within a brand — the interface, the matrix, the supported effect
identifiers. What does not vary — the report structure, the checksum
range — stays in the family's code, in a single place, tested
once against a captured frame.

The two contribution costs then become very different, and that is
desirable:

| Contribution | Cost |
|---|---|
| a device from a known family | **data** — reviewable line by line |
| a new family | code, and a captured frame to anchor it |

⚠️ **The "almost" family trap**. A model whose checksum
covers a different range, or whose report is 64 bytes, is not a parameter
variant: it is another family. A data layout that was wrong
would be accepted by the device, return `Ok`, and light nothing — see §8. The
provenance `method: inferred` exists for exactly this case.

---

## 8. What cannot be verified without the hardware

This is the most uncomfortable question in [#34], and the only one whose honest
answer is "a lot of things".

### The foundation: accepted is not understood

`reachingKeyboard` proves the write was **accepted**, not that it was
**understood**. The survey demonstrates it live: the device validates the
class/command pair, **not the value of the arguments**. Effect identifiers `0x05`
and `0x07` are accepted — status `0x02`, "understood" — and leave the effect
**unchanged**. An absurd argument size gets through too.

Everything that follows stems from this: on a device we do not have, **no layer
reports a wrong layout**. The write succeeds, the indicator is green, and nothing
lights up.

### The list, unsoftened

| Unverifiable without the hardware | What it gives when it is wrong |
|---|---|
| the interface does carry the lighting | **valid** handle, every write lost |
| the bytes are understood | `Ok`, device frozen |
| the frame covers the whole matrix | the last rows stay frozen — the 132 / 106 trap |
| the geometry looks like the device | a wrong simulator, that only the eye can disprove |
| the scancodes match the key positions | an effect that lights the wrong key |
| the frame rate holds | frames silently dropped |
| no command breaks something else | the driver mode of §6 |

### What continuous integration can establish

And it is less meager than it seems, provided the right piece is put in.

**The captured frame attached to the layout.** This is already what
`checksum_matches_captured_frame` does: a row actually observed on the bus, its
expected checksum, and a test that rebuilds the frame and compares. A
contribution brings at least one. It does not prove the device obeys —
it proves that **the repository code reproduces what the contributor saw**, which
is exactly the half that can be held without the hardware.

The rest is verified through internal consistency, and the existing tests in `layout.rs`
already provide the model — unique indices, bijection between matrix and keys,
disjoint rectangles, counts per row. Generalized to every layout, they
catch the most likely mistake of a hand transcription: a key shifted
by one.

On top of that comes the consistency between data and capabilities, which becomes
verifiable because capabilities are read and not declared: a layout that claims
`geometry` without rectangles is not refused at runtime, it is **impossible to write**.

### What reviewing a contribution means

The consequence for the reviewer is direct, and it is better to write it down than
let it be discovered:

> **One does not review the accuracy of a survey. One reviews its consistency
> and its provenance.** Saying yes to a hardware contribution means saying "this is
> consistent, dated, and reproducible by someone who had the device" —
> never "this is right".

The questions a reviewer can answer:

- is the attached frame **rebuilt** by the code, checksum
  included?
- is the provenance complete, and does `firmware_read_by` name something other
  than `release_number`?
- does `origin` distinguish what is surveyed from what is drawn?
- do all writes go through the SDK intents (§6)?
- does a command go beyond lighting? If so, **why**?
- are the capabilities actually read from the data, and not asserted?

And those they cannot answer — which they must therefore not pretend to
settle: does the device obey, does the drawing look like it, are
the names the right ones.

### What the user needs to know

A device whose survey has been reproduced by nobody other than its author
must not present itself like the others. Provenance is already on screen
([#35]); it only needs to also say **who has seen this device work**.

And adoption stays what it is: the default is `detected`, never `adopted`.
"Writing to a USB device one poorly understands is not harmless"
([`effects-runtime.md`](effects-runtime.md) §7) — it is even more true of a
device whose layout comes from elsewhere.

---

## 9. Loading: compiled in, not a plugin

A plugin that writes over USB is an attack surface, and [#34] asked to
decide rather than assume. Yet the reason for deciding on **compiled in** is not
security first:

**A plugin would escape the only verification we have.** All of §8
rests on tests run by continuous integration on the repository's
content: the replayed frame, the matrix consistency, the presence of the
provenance. A layout loaded at runtime has gone through none of these tests,
and nobody has reviewed it. All that would remain of the contribution is its
most costly part — writing to a bus — without any of what makes it acceptable.

On top of that, the PR **is** the loading mechanism: it carries the review,
the reference frame, the provenance and the history. That is what made
OpenRGB grow, and it is no accident.

To be reopened if, and only if, the number of layouts makes compiling the
catalog unreasonable. That is not a problem we have with one
device, nor with twenty.

---

## 10. What this changes in the existing code

Described, not written: the implementation is the body of [#34].

- **`Layout` gains a kind and a provenance**, and its grid becomes
  optional — a device without a neighborhood has no `rows` and `cols` to
  invent. `led_count` becomes "grid positions + zones", and the contract
  "a frame covers all positions" does not move.
- **`Key` has its name and its rectangle become optional**, which is the
  condition for `namedKeys` and `geometry` to be read instead of declared.
- **`DeviceOut` does not change.** The engine already receives an abstract output and a
  layout, never a `Keyboard`: that seam is what makes this whole document
  additive.
- **The effects API gains `kinds` and `requires`**, mandatory, and its `Key`
  gains a zone role. The shipped effects gain one line each. Its `Key`
  already carries the rectangle, optional, since [#60]: that is the "effect
  side" half of `geometry`, and it did not wait for the rest.
- **The manifest**, read by running the module ([`effects-library.md`](effects-library.md) §3),
  must carry these two fields the way it carries `params`, and refuse a malformed
  one with a sentence rather than discover it at the first frame.
- **The simulator** must know how to draw something other than an ISO keyboard: a
  grid of squares without geometry, named dots for zones.

---

## 11. Open questions

- **The language of identifiers.** This document proposes `kinds`, `requires`,
  `geometry`, `wheel` — English, to stay consistent with `name`, `params`, `render`,
  `rows`, `cols` and the rest of the public surface. The prose stays in French. To
  be confirmed, because it is as costly to change later as the rest of the
  vocabulary.
- **Do zones and the grid really coexist?** A keyboard with
  underglow would have both, and a flat frame would concatenate both. Nothing
  here stands in the way, and nothing has tried it either.
- **Intercepting undeclared reads** (§4) is a lead, not a
  decision. Its real cost in QuickJS has not been measured.
- **Can a protocol family be described as data?** §7 leaves it in code.
  The exact boundary between "family parameter" and "new family" will only
  be settled with the second protocol, and not before.
- **Two units of the same model remain indistinguishable** when the USB
  descriptor carries no serial number ([#35]). The SDK changes nothing there, but a
  catalog of layouts makes the case more frequent.
- ~~**A physical radial wave** does not exist yet~~ — it has existed since
  [#60], and the `geometry` term is now backed by a file. What it shows
  along the way: unable to **require** the capability, it throws on the
  first frame rather than being kept out of the gallery. The message names the
  key without a rectangle, so nothing is silent — but it is a refusal that
  comes too late, and that is exactly the half [#34] must close.

---

## 12. What this document does not solve

The survey remains to be done by someone who owns the device. No vocabulary
creates knowledge: it only prevents it from being lost for lack of a place
to put it, and from being wrong for lack of having been dated.

[#34]: https://github.com/oorabona/candeo/issues/34
[#35]: https://github.com/oorabona/candeo/issues/35
[#44]: https://github.com/oorabona/candeo/issues/44
[#60]: https://github.com/oorabona/candeo/issues/60
