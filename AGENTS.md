# Working on Candeo

Rules for anyone changing this repository, people and coding agents alike.
The README explains what Candeo is; `docs/design/` records why it is built
the way it is.

## Layout and commands

- `crates/candeo-protocol`: report construction, pure and tested without hardware.
- `crates/candeo-device`: HID transport and device layouts.
- `apps/desktop`: the Tauri application (Vue 3 + TypeScript, Rust in `src-tauri`).
- `packages/effects-api`: TypeScript types for effect authors.

Run before pushing, the same checks as CI:

```bash
cargo fmt --all -- --check
pnpm lint:rust        # cargo clippy --workspace --all-targets -- -D warnings
pnpm test:rust        # cargo test --workspace
pnpm --filter @candeo/desktop exec vue-tsc --noEmit
pnpm test:web         # vitest run, in apps/desktop
```

Front-end tests sit next to the module they cover (`useSettings.test.ts`), run
in Node without a DOM, and mock `api/candeo.ts` rather than Tauri itself.
Composables with lifecycle hooks run through `src/test/withSetup.ts`.

Run the app with `pnpm tauri dev`. Test a change in the running app, not only
through the test suite, before opening a pull request.

## Languages

**English everywhere, except text shown to the user.**

| What | Language |
|---|---|
| Identifiers, comments, test names, assertion messages | English |
| Log messages | English |
| Docs, commit messages, issues, pull requests | English, interface elements included: *Refresh*, not the French label |
| Text shown to the user | Through the i18n catalogs, never inline |

Code written before this rule still holds French identifiers, comments and log
messages. It is being migrated; **never add new French to code**, and do not
mix both languages inside a new function or type.

## Internationalisation

Introduced by #73. Every screen, the tray and command errors go through the
catalogs; new interface text goes there too.

- **Catalogs:** `apps/desktop/src/locales/en.json` is the reference, `fr.json`
  a complete translation. Missing keys fall back to English.
- **Keys** are namespaced by screen or feature: `devices.adopt`, `tray.quit`,
  `errors.deviceNotOpen`. Placeholders are named (`{name}`); never build a
  sentence by joining translated fragments.
- **Front end:** `vue-i18n`, used through `t` from `src/i18n`, whose keys are
  typed from `en.json`: an unknown key fails `vue-tsc` (vue-i18n's own `t`
  accepts any string). No raw text in templates.
- **Command errors** cross the IPC boundary as a `Failure` (`src-tauri/src/failure.rs`):
  a code under `errors.` with parameters, which `message()` in `api/journal.ts`
  translates. Its `Display` is the English text, for the log. An error nobody
  can act on is `Failure::unexpected` with an English detail.
- **Text rendered by Rust** (the tray menu, device warnings, copy names) uses the
  same JSON catalogs, embedded with `include_str!` and read through
  `i18n::t(language, key, params)`.
- **English only:** logs, the copied diagnostic, and what an effect's author
  reads: load and render errors, like TypeScript's diagnostics.
- **Language setting:** `language` in `settings.json`, `"system"` by default,
  or `"en"` / `"fr"`, resolved in Rust (`src-tauri/src/language.rs`): the
  display language on Windows (`GetUserDefaultUILanguage`), `LC_ALL`,
  `LC_MESSAGES` or `LANG` elsewhere; an unsupported language falls back to
  English. Changing it re-renders the window and rebuilds the tray menu.
- **Effects:** an effect's name is its file name and is not translated. Its
  `description` and parameter `label`s accept a string or a map of languages
  (`docs/design/effects-library.md` §2); they do not go through the catalogs.
- **Guards:** `vue-tsc` checks `fr.json` against the shape of `en.json`, a test
  checks that they have the same keys, and a Rust test reads the sources for
  every key and `Failure` code used from Rust.

## Interface text

- **The interface shows states and actions.** Why the app works the way it
  does belongs in `docs/`, not on screen.
- **A warning only for what is irreversible or surprising**, said once, in one
  place.
- **An error is one sentence and the action to take.** Technical detail goes to
  the log and the copied diagnostic, never raw into the window or the tray.
- **One term per state:** *controlled*, *not open*, *unplugged*; the action is
  *Control* (*piloté*, *non ouvert*, *débranché*, *Piloter* in the current French
  interface). Settle a new term before using it anywhere.
- The copied diagnostic is for bug reports: it stays in English and does not go
  through the catalogs.

## Code and comments

- **Comments explain why, not what:** the constraint, the measurement or the
  incident that justifies the code. Keep measured numbers with their source.
- Decisions that shape several modules are written in `docs/design/` before the
  code.
- Keep decisions testable: extract the logic into pure functions and cover it
  with unit tests, so a scenario needing hardware we do not have is still tested.

## Hardware safety

- A status byte `0x02` validates the class and command, **never argument
  values**. Verify a write by reading it back.
- Every protocol fact in `docs/protocol/` carries the firmware version and the
  date it was established.
- Class `0x00` (device information) is **read only**. Never switch the device to
  driver mode (`0x00`/`0x84`): the firmware stops handling some keys, for nothing
  a lighting controller needs.
- Probes in `apps/desktop/src-tauri/src/sonde.rs` are `#[ignore]`. Run one only
  with the app closed and someone watching the keyboard.

## Privacy

Never commit or publish serial numbers, machine or user names, local paths,
IP addresses or device instance IDs: not in code, docs, logs, tests, commits,
issues or pull requests. Logs identify a unit by its serial fingerprint, never
the serial itself. Test fixtures use made-up values such as `XY24ABCDEFG0001`.

## Git and pull requests

- Branch from `origin/main` as `type/short-name`. Never push to `main`, never
  force-push a shared branch.
- **Commits:** Conventional Commits in English, imperative mood, subject under
  72 characters, **50 words at most** in total. One logical change per commit.
  No attribution trailers. The detailed reasoning goes in the issue or pull
  request, not in the commit.
- **Pull requests:** English, with *What changed*, *Tests* and *Not verified*
  sections, and `Fixes #N` when they close an issue. Re-read the published text.
- **Pull request titles in plain language**, not as a Conventional Commit:
  *Release pipeline with release-please*, not `ci: release pipeline`. The merge
  commit carries the title, and release-please would list the change a second
  time in the changelog. The release pull request is titled
  `Go release vX.Y.Z 🚀`.
- CI skips pull requests limited to Markdown, `docs/` and the licence; on `main`
  everything always runs. The Linux packaging job runs on `main` and on demand
  only.
- **Releases** come from release-please: never bump a version or tag by hand.
  See [`docs/releasing.md`](docs/releasing.md).
