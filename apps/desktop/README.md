# Candeo — desktop application

The window and its Tauri host. The rest of the workspace lives elsewhere:
`crates/candeo-protocol` builds the HID reports, `crates/candeo-device` sends
them, `packages/effects-api` describes the API offered to the effect author.

```
src/         la fenêtre — Vue 3, TypeScript, l'éditeur Monaco
src-tauri/   l'hôte Rust — commandes, moteur d'effets, journal, stockage
```

## Running

From the **root** of the repository, not from here — the scripts go through the pnpm
filter and the paths in `tauri.conf.json` are relative to `src-tauri/`:

```sh
pnpm dev             # fenêtre en développement, rechargement à chaud
pnpm build           # vue-tsc puis vite build
pnpm tauri build     # application empaquetée
pnpm check           # clippy et tests Rust
pnpm test:web        # front-end tests (Vitest)
```

## Where to read next

- [`README.md`](../../README.md) at the root — what Candeo does, and why.
- [`docs/design/studio.md`](../../docs/design/studio.md) — the architecture
  decisions for the window and the editor.
- [`docs/design/effects-runtime.md`](../../docs/design/effects-runtime.md) — the
  effect engine, the render loop and the path frames take.
- [`docs/api/commands.md`](../../docs/api/commands.md) — every Tauri command,
  its arguments and its failure modes.
- [`docs/protocol/`](../../docs/protocol/) — the protocol survey, from which everything
  else derives.
