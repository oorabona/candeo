# Effect sources — shipped effects in the application, yours in Documents

Status: **implemented** (#125). Changes §1, §4, §5 and §6 of
[`effects-library.md`](effects-library.md), which described every effect in one
folder, keyed by its name.

## Why

Every effect lives in `app_data_dir()/effects/`: the shipped ones, which Candeo
seeds and updates, and the ones people write, duplicate or drop in.

- **Uninstalling can delete people's work.** The NSIS uninstaller offers to
  delete the application data. An MSIX package, the Microsoft Store listing of
  #126, removes it without asking.
- **An MSIX package splits the library.** Files and folders a packaged app
  creates under `AppData` go to a location private to the package. The app sees
  them merged with the real `AppData`; no other process does. The effects Candeo
  seeds, duplicates or saves from its editor would be invisible to the file
  manager and to other editors, while files dropped in from outside stay
  visible. **Open folder** would show part of the library.
- Turning that redirection off takes the `unvirtualizedResources` restricted
  capability, which Microsoft reserves for narrow cases and reviews separately.
- `Documents` is where people look for what they made, back it up and sync it.

The two kinds were already told apart: the gallery's **Built-in** and **Yours**
sections, and `shippedEffects` in `settings.json`, which exists only because they
shared a folder.

## 1. Two sources

| Source | Folder | Holds | Written by |
|---|---|---|---|
| `shipped` | `app_data_dir()/effects/` | the effects Candeo ships | Candeo, at startup and on **Restore** |
| `user` | `document_dir()/candeo/effects/` | the effects people write, duplicate or drop in | people and the editor |

- **Same layout on every system**, through Tauri's `document_dir()`: `Documents`
  on Windows, the XDG documents directory on Linux. The user folder is created
  when first needed.
- **Fallback:** where `document_dir()` does not resolve, the user folder is
  `app_data_dir()/user-effects/`, and the log says so once.
- **Cache:** `app_cache_dir()/effects/<source>/<name>.json`, one tree per source.
- **Under MSIX:** the shipped folder, the cache, `settings.json` and the logs stay
  in `AppData`, private to the package and removed with it, which suits what
  only Candeo reads. The user folder is outside `AppData`: visible to every
  program, and kept on uninstall. No restricted capability.

## 2. Identity: source and name

- **The key is `<source>:<name>`**: `shipped:Breathing`, `user:My effect`. It
  replaces the bare name as the key of `activeEffects` and `effectParams` in
  `settings.json`, the engine, the tray menu and editor drafts.
- **Hardware effects keep `hardware:wave`**, which already has this shape: a third
  source, with no folder.
- **The key is unambiguous:** allowed names exclude `:` (§1 of
  `effects-library.md`), so the source is everything before the first `:`.
- **Shown:** the file name without `.ts`, as today, in the section of its source.
  The tray menu lists effects in the same groups as the gallery, under a
  **Built-in** and a **Yours** heading when both have effects, so two effects of
  the same name are never side by side unlabelled.
- **A shipped effect and a user effect may share a name.** They are two effects
  with two keys, and there is no conflict to report. Within one source, the file
  system keeps names unique, and names that differ only by case stay refused.
- **Not the absolute path**, although a path is what makes a file unique:
  - it carries the user name into logs, the copied diagnostic and `settings.json`;
  - it breaks every reference when `Documents` moves: OneDrive folder
    redirection, another machine, a change of application identifier.
  - A path relative to its source's folder has neither problem.
- **Subfolders:** not listed now. The key leaves room for them, as
  `user:packs/Rain`.

## 3. Where each gesture writes

| Gesture | Source written |
|---|---|
| New effect, Save as | `user` |
| Duplicate, of either source | `user` |
| Rename | `user` only; shipped effects stay read-only in the application |
| Delete | `user` only |
| Open folder | opens the user folder, created if missing |
| Restore original, Restore built-ins | `shipped` |

A shipped effect edited from the file manager is still marked modified, with
**Restore original**, where its folder is reachable. Under MSIX it is not, and
Duplicate is the way to change one.

## 4. Shipped effects

The seeding table of `effects-library.md` §4 applies to the shipped folder, with
three changes:

- **"Not recorded, a file with that name exists"** no longer leaves a user's file
  among shipped ones: see the migration, which empties the shipped folder of
  everything Candeo did not put there.
- **"Recorded, no longer shipped by this version":** the file moves to the user
  folder, and its references are rewritten from `shipped:` to `user:`. On a name
  already taken there, it takes a ` (2)` suffix.
- **"A user effect created onto the name of a missing built-in takes the name"**
  disappears: the two live in different folders.

`shippedEffects` stays, keyed by name: it describes the shipped folder.

## 5. Migration

One pass at startup, idempotent, recorded as `version: 3` in `settings.json`
(version 2 was the move from former built-in ids to names):

1. **User folder:** created.
2. **User files:** every `.ts` in `app_data_dir()/effects/` not recorded as
   shipped, or recorded as not ours, moves there, with a ` (2)` suffix on a name
   already taken. The records of files that were not ours are removed from
   `shippedEffects`.
3. **References:** `activeEffects` and `effectParams` are rewritten. A recorded
   shipped name becomes `shipped:<name>`, a moved file `user:<its name after the
   move>`; `hardware:*` is unchanged.
4. **Cache:** the flat cache files are deleted, and effects are compiled again on
   the next listing.
5. **Drafts:** `candeo:brouillon:<name>` is renamed to its key when the editor
   opens: `shipped:` for a name recorded as shipped, `user:` otherwise. The user
   folder is new, so a move renamed by a collision needs a folder someone made by
   hand; that file's draft is then left under its old name.

**Order and failures.** As in the earlier migrations, moves are planned from what
is on disk, `settings.json` is rewritten, then files move. A file that cannot be
moved stays where it is and the failure is logged; the other files still move,
and the next launch plans the same move again. A copy already made with the same
bytes is recognized, not copied twice. A move across volumes (`Documents` on
another drive, or in OneDrive) copies then deletes.

**Every launch.** The move of files Candeo did not put in the shipped folder runs
at every startup, after seeding: a file dropped among the shipped effects, or one
this version no longer ships, moves then. Until it has moved, such a file is not
listed. Seeding runs again after the move, so a shipped effect whose name a moved
file held is copied in the same launch.

## 6. OneDrive and sync

`Documents` is often redirected to OneDrive:

- **Online-only files:** hashing one to list it downloads it. That is acceptable,
  since listing happens at startup and on **Refresh**.
- **Sync conflicts:** a conflict copy appears as one more effect, which is what
  the folder holds.
- **Placeholder that cannot be read** (offline): the effect is listed with an
  error state, like a file that does not load.

## 7. Out of scope

- Choosing the user folder in Settings.
- Listing subfolders.
- Watching either folder for changes.
- The MSIX package itself (#126).
