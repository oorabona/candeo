# The effects library — effects are files

Status: **implemented**. Replaces the uid-based version accepted in #83, which was
set aside during implementation review for a simpler model. Covers the shipped
effects and the identity and format of #44.

> **Changed since** (#125): shipped effects stay in this folder, the user's live
> in `Documents`, and an effect's key is `<source>:<name>`. See
> [`effects-sources.md`](effects-sources.md), which changes §1, §4, §5 and §6
> below.

## Why

Candeo ships five effects as JavaScript compiled into the binary, and treats
them as a second kind of effect. That kind costs a special case almost
everywhere: reserved ids, refused deletion, copy-before-editing, manifests
written twice, swatches kept in memory, `EffectKind::Builtin` checks in storage,
listing and the gallery.

Meanwhile #44 needs an identity and a single-file format for effects that come
from elsewhere. The first design answered with a generated uid written into the
source. In review it proved heavier than the need: a uid the user can edit is
a way to overwrite another effect by pasting its code, and it adds a second
identity next to a name that is already unique.

**Decision: an effect is a `.ts` file in the effects folder, and its file name
is its name.** Candeo is a tool for power users: listing a folder is the whole
library, adding an effect is saving a file there, and naming it is renaming
the file.

## 1. Identity: the file name

```text
app_data_dir()/effects/
  Radial wave.ts
  Breathing.ts
  My effect.ts
```

- The name shown everywhere is the file name without `.ts`. Sources no longer
  declare `name`.
- It is the key of everything that refers to an effect: `activeEffects` and
  `effectParams` in `settings.json`, the engine, the tray menu, editor drafts.
- **Allowed names** follow the Windows rules on every system, so a folder copied
  between machines means the same thing: no `< > : " / \ | ? *` or control
  characters, no leading or trailing space or dot, not a reserved device name
  (`CON`, `NUL`, `COM1`…), at most 64 characters. Two names that differ only by
  case are refused.
- **Renaming from the app** renames the file and moves its settings and draft.
  **Renaming outside the app** makes a new effect: the settings of the old name
  stay in `settings.json`, unused. See §5.
- **Hardware effects** (`wave`, `off`, `spectrumCycle`, defined in the front
  end) move to ids with a prefix no file name can hold, `hardware:wave`, so a
  file named `wave.ts` cannot collide with them. The tray menu reads an effect
  id as everything after the product id, so the `:` does not break it.

## 2. Format: one self-contained `.ts` file

```ts
import { defineEffect, hsv } from '@candeo/effects-api'

export default defineEffect({
  description: { en: 'A hue wave spreading in circles', fr: 'Une onde de teinte en cercles' },
  kinds: ['keyboard'],
  params: {
    speed: { kind: 'number', label: { en: 'Speed', fr: 'Vitesse' }, min: 0, max: 400, default: 120 },
  },
  render({ layout, time, frame, params }) { … },
})
```

- **Text fields** — `description`, parameter `label`, and a `choice` option's
  `label` (`{ value, label }`, or a plain string shown as it is) — accept a
  string or a map of languages. The display picks the current language, then
  English, then the first entry. The name is not translated: it is a file name.
- **`kinds`** declares the device kinds the effect is meant for, as decided in
  #44 §5 and `device-sdk.md` §2 (`['keyboard']`, `'all'`…). Shipped effects
  declare it now. Only keyboards exist today, so a missing `kinds` is read as
  `['keyboard']`; the obligation and the gallery filter come with the second
  kind.
- `author` and `version`, proposed in #44 §1 as optional, are not implemented
  yet: see #44.
- `name` in an existing source is **ignored**. `EffectModule` keeps it as a
  deprecated optional property, so an old source still type-checks and the
  editor strikes it through instead of refusing to save.
- Defaults no longer need to be literals: the manifest is read by running the
  module (§3), not from the syntax tree.

## 3. Compiling and the cache

The Rust side cannot strip TypeScript types, and the webview already can: the
editor loads the TypeScript compiler for Monaco. The main window is created at
startup and closing it only hides it, so a webview is always there while Candeo
runs, tray-only use included.

1. **Rust lists the folder**: for each `.ts`, its name, the SHA-256 of its bytes,
   and whether the cache holds a result for that hash.
2. **The webview compiles what is stale**, with `transpileModule`, at startup
   and on **Refresh** in the gallery. The compiler is loaded only when at
   least one file is stale.
3. **Rust receives the JavaScript**, loads it once in QuickJS with the time
   budget swatch sampling already uses, reads the manifest the module declares
   (`__candeo_manifest`), checks `apiVersion`, samples the swatch, and writes
   `app_cache_dir()/effects/<name>.json`: hash, JavaScript, manifest, swatch.
4. A file that fails to compile or load is cached **with its error**, for that
   hash: it is listed with an error state, opens in the editor, and is not
   retried until it changes.

The effects folder holds only the files people write. The engine and the tray
run an effect only when its cache matches the file's current hash, so they
never run code that differs from the file on disk. A file dropped in while
Candeo runs appears after Refresh; watching the folder can come later.

What goes away: the manifest reader on the syntax tree (`editor/effect.ts`), the
`source.ts` / `effect.js` / `manifest.json` / `swatch.json` directory per
effect, and option A of the first design (generated JavaScript committed and
checked by CI).

## 4. Shipped effects

The sources move to `packages/effects/<Name>.ts`, type-checked by `vue-tsc`, and
are embedded in the binary. They carry **no type annotations**: `defineEffect`
already infers what `render` receives, and the Rust tests run them as they are,
without a TypeScript compiler, to check the waves' geometry and the swatches. A
test fails the day one of them would need its types stripped. At startup they are copied into the effects folder
**once**, and `settings.json` records what was copied:
`shippedEffects: { "Radial wave": "<sha-256>" }`.

Shipped effects are named in English: a name is a file name and is not
translated, and English is the reference language of the interface.

| State at startup | Action |
|---|---|
| Not recorded, no file with that name | copy it, record its hash |
| Not recorded, a file with that name exists | leave the file; record it as not ours |
| Recorded, file unchanged, shipped version changed | overwrite it **silently**, record the new hash |
| Recorded, file modified | leave it |
| Recorded, file missing (deleted or renamed) | leave it; never copied again |
| Recorded, no longer shipped by this version | forget the record; the file becomes the user's |

The gallery's **Built-in** section lists the files whose name is recorded as
shipped, modified or not. Resetting the configuration keeps `shippedEffects`: it
describes the folder, not a preference, and losing it would turn every built-in
into the user's at the next launch.

### Built-ins are read-only in the application

Deleting, renaming or saving over a built-in is refused, in the window and in
Rust. Duplicate makes an editable copy, which is the user's. Two reasons:

- an edited copy no longer receives updates (the table above), without anything
  saying so;
- a renamed or deleted one never comes back, so the three gestures lose the same
  thing.

The folder stays the user's: a built-in edited, renamed or deleted from the
file manager is dealt with below, not prevented.

- **Modified**: a built-in whose file no longer has the recorded hash is marked
  modified, with **Restore original**, which overwrites the file after a
  confirmation.
- **Missing**: shipped effects with no file of their name are offered back with
  **Restore built-ins**, which copies them and records them again, so updates
  resume. Never at startup: removing a built-in from the folder must stay
  possible.
- A user effect created or renamed onto the name of a missing built-in takes the
  name: the record becomes "not ours", and restoring that built-in is refused
  until the name is free.

## 5. Library actions

- **Refresh**, and **Open folder**, in the effects column.
- **Rename** from the editor header: renames the file, moves its settings and
  its draft. The name is edited there only; sources have no `name`.
- **Duplicate**: a copy in the user's folder, under the original's name when that
  folder holds none — a built-in's first copy — and otherwise numbered,
  `<name> (2)`, `(3)`…, the way every name made free is. A file name does not
  change with the interface language. Ready at once, since its cache is copied
  too, and the user's even when the original was shipped.
- **Delete**, the user's effects only: stops the loops running it, removes the
  file and its cache, forgets its settings.
- **Missing effect**: when `activeEffects` or `effectParams` name an effect the
  folder no longer holds, the gallery says so once — "Radial wave" is no
  longer in the folder — with **Forget its settings**, and **Restore** when it is
  a built-in. Nothing is purged automatically: putting the file back under that
  name restores everything.
- **Restore built-ins** and **Restore original**: see §4.

## 6. Migration from the directory layout

One pass at the first launch of the new version, idempotent, recorded as
`version: 1` in `settings.json`:

1. Each `effects/<id>/` becomes `effects/<name>.ts`, `<name>` being the
   manifest's name made valid (forbidden characters replaced by `-`, a suffix
   ` (2)` on collision). The directory is removed once the file is written.
2. `activeEffects` and `effectParams` are rewritten from old ids to names:
   installed effects through step 1, shipped effects through a table
   (`onde-radiale` → `Radial wave`, `onde-matricielle` → `Diagonal wave`,
   `respiration` → `Breathing`, `balayage` → `Sweep`, `degrade-fixe` →
   `Fixed gradient`).
3. Shipped effects are then copied as in §4.
4. Drafts stored under `candeo:brouillon:<id>` are renamed when the editor
   opens, from the same table.

Tested on a fixture shaped like a data folder of the directory layout.

## 7. What disappears, what stays

Disappears: `builtins/mod.rs`, reserved ids, the refused deletion, memory-only
swatches, the Rust manifest copies and their test, copy-on-open in the editor,
`derive_id`, per-effect directories.

`EffectKind` stays, with another meaning: `builtin` marks a file recorded as
shipped, which the gallery lists in its Built-in section and the application does
not delete, rename or overwrite (§4).

Stays: one engine, one API, the swatch sampled by running the effect, per-device
settings, preview versus Apply.

## Trust: the folder is the boundary

Any `.ts` file in the effects folder runs: its module body and a few frames when
it is compiled, then every frame while it is applied or previewed. There is no
import step that asks first, and none is needed while the folder is the only
way in: whoever can write there can already run code as the user.

What an effect can reach is bounded by the engine, not by trust:

- **QuickJS, no host API**: no file, network, process or clock beyond `time`;
  only the layout, its parameters, and the inputs it declares.
- **Limits** on computing time per frame and at load, and on memory (#49).
- **Frames only go out**: the host clamps colors and ignores writes outside the
  frame.
- **Key presses** reach only an effect declaring `inputs: ['keys']`, and the
  error text of such an effect stays out of the log and the copied diagnostic,
  since it could carry them (`key-input.md` §3).

A shared catalog — effects fetched rather than dropped in by hand — would move
the boundary, and is the moment to reconsider the engine (`boa_engine`) and an
import step that shows what an effect declares before it first runs.

## 8. Out of scope

- Watching the folder for changes.
- An "update available" mark for modified shipped effects.
- The `kinds` filter and obligation (with the second device kind, #34).
- The OpenRGB Effects Plugin reimplementations: written directly in this format
  once it lands.

## Order of work

1. File-named effects, the compile pass and cache, Refresh, the migration,
   settings keyed by name.
2. Shipped effects as `packages/effects/*.ts`, seeding; removal of the built-in
   special cases.
3. Rename, Duplicate, Open folder, the missing-effect notice.
4. Localized `description` and `label` (with #73).
