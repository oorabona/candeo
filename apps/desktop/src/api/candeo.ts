/**
 * Seul point de passage vers le Rust.
 *
 * Aucun composant n'appelle `invoke` directement : tout transite par ici, pour
 * que la surface soit typée en un seul endroit et que le jour où une commande
 * change de forme, le compilateur désigne les appelants.
 */

import { Channel, invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { ParamSpec, ParamValue, Text } from '@candeo/effects-api'

import type { DeviceInfo, DeviceRef, DeviceState, Effect, Failure, LayoutInfo, Rgb } from './types'

/** Liste les gabarits connus, branchés ou non, avec l'état de chacun. */
export function listDevices(): Promise<DeviceInfo[]> {
  return invoke('list_devices')
}

/**
 * Retient « piloté » pour cet appareil, et l'ouvre s'il est là.
 *
 * La décision est écrite **avant** l'ouverture et tient même si celle-ci
 * échoue : les démarrages suivants la rejoueront. Rend le gabarit quand
 * l'appareil a été ouvert, `null` quand il est adopté mais débranché — ce n'est
 * pas une erreur.
 */
export function adoptDevice(vid: number, pid: number): Promise<LayoutInfo | null> {
  return invoke('adopt_device', { vid, pid })
}

/** Retient « ignoré », et referme l'appareil s'il était ouvert. */
export function ignoreDevice(vid: number, pid: number): Promise<void> {
  return invoke('ignore_device', { vid, pid })
}

/**
 * Ouverture ponctuelle, sans rien décider.
 *
 * Ne touche pas à `settings.json`, donc ne survit pas au redémarrage —
 * contrairement à {@link adoptDevice}.
 */
export function connect(vid: number, pid: number): Promise<LayoutInfo> {
  return invoke('connect', { vid, pid })
}

/**
 * Referme **un** appareil. Les autres ne sont pas touchés.
 *
 * Avec {@link connect}, la paire qui ouvre et referme **sans décider** — là où
 * {@link adoptDevice} et {@link ignoreDevice} écrivent dans `settings.json`.
 * `useDevice` les enveloppe toutes deux ; aucun écran ne les appelle encore.
 */
export function disconnect(device: DeviceRef): Promise<void> {
  return invoke('disconnect', { device })
}

/** Gabarit d'un appareil ouvert. Échoue s'il ne l'est pas. */
export function getLayout(device: DeviceRef): Promise<LayoutInfo> {
  return invoke('get_layout', { device })
}

/**
 * Gabarit de repli, quand rien n'est connecté.
 *
 * `getLayout` refuse hors connexion, et c'est le cas qu'il faut servir : on
 * dessine le clavier et on écrit un effet avant d'avoir branché quoi que ce
 * soit. La géométrie reste ainsi définie **au seul endroit** où elle est
 * testée, dans `crates/candeo-device`.
 */
export function getDefaultLayout(): Promise<LayoutInfo> {
  return invoke('get_default_layout')
}

/**
 * Luminosité **pleine** : le défaut d'un clavier qu'on vient de brancher.
 *
 * Miroir de `BRIGHTNESS_DEFAUT` dans `src-tauri/src/storage.rs`. C'est aussi la
 * valeur que `settings.json` n'écrit pas — la retenir revient à retirer l'entrée.
 */
export const BRIGHTNESS_DEFAULT = 255

/**
 * Écrit la luminosité **sur le clavier**, et rien d'autre. `level` de 0 à 255.
 *
 * Ne retient rien : c'est {@link rememberBrightness}. Même séparation que
 * {@link setEffectParams} et {@link rememberEffectParams}, pour la même raison —
 * un curseur qu'on glisse produit des dizaines d'écritures HID et une seule
 * écriture disque, quand il s'arrête.
 */
export function setBrightness(device: DeviceRef, level: number): Promise<void> {
  return invoke('set_brightness', { device, level })
}

/**
 * Retient la luminosité de cet appareil, sans toucher au clavier.
 *
 * Elle est réappliquée à l'adoption et au démarrage : un niveau retenu qui ne se
 * réapplique pas au branchement ne servirait à rien, et le protocole relevé sait
 * écrire la luminosité mais pas la relire.
 *
 * {@link BRIGHTNESS_DEFAULT} **efface** l'entrée, comme une table de paramètres
 * vide efface les réglages d'un effet.
 */
export function rememberBrightness(device: DeviceRef, level: number): Promise<void> {
  return invoke('remember_brightness', { device, level })
}

/**
 * Bascule l'effet matériel d'un appareil.
 *
 * Tout sauf `custom` est exécuté par le **micrologiciel** : coût processeur
 * nul, et l'effet survit à la fermeture de l'application.
 */
export function setEffect(device: DeviceRef, effect: Effect): Promise<void> {
  return invoke('set_effect', { device, effect })
}

/**
 * Pousse une image complète.
 *
 * `frame` doit compter exactement `layout.frameLen` couleurs — **toutes** les
 * cases de la matrice, y compris celles sans LED. En envoyer moins laisse les
 * dernières rangées figées sur leur valeur précédente.
 *
 * ⚠️ **La fenêtre n'envoie pas d'images.** C'est la boucle Rust qui les produit
 * et les écrit ; le simulateur les **reçoit** par canal. Cette enveloppe et
 * {@link writeRow} sont les primitives de bas niveau qui ont servi à établir le
 * protocole, gardées pour pouvoir le refaire — le pourquoi est écrit sur les
 * commandes, dans `src-tauri/src/lib.rs`. Les appeler depuis un écran
 * entrerait en concurrence avec la boucle sur la même poignée HID.
 */
export function present(device: DeviceRef, frame: readonly Rgb[]): Promise<void> {
  const flat = new Array<number>(frame.length * 3)
  for (let i = 0; i < frame.length; i++) {
    flat[i * 3] = frame[i][0]
    flat[i * 3 + 1] = frame[i][1]
    flat[i * 3 + 2] = frame[i][2]
  }
  return invoke('present', { device, frame: flat })
}

/** Écrit un segment de rangée, sans toucher au reste. */
export function writeRow(
  device: DeviceRef,
  row: number,
  colStart: number,
  colors: readonly Rgb[],
): Promise<void> {
  const flat = colors.flatMap((c) => [c[0], c[1], c[2]])
  return invoke('write_row', { device, row, colStart, colors: flat })
}

// ---------------------------------------------------------------- bibliothèque

/**
 * Version de l'API d'effets que cette application sait servir.
 *
 * Miroir de `EFFECTS_API_VERSION` dans `src-tauri/src/storage.rs`, au même
 * titre que les types de `api/types.ts` le sont de `lib.rs`. La divergence n'est
 * pas silencieuse : un manifeste qui annonce une version plus récente que celle
 * du Rust est **refusé à l'installation**, avec un message qui le dit.
 *
 * Elle ne vient pas de `@candeo/effects-api` : ce module décrit l'API, il ne se
 * numérote pas lui-même — et tout ce qu'il exporte doit exister dans le jumeau
 * `src-tauri/src/runtime/api.js`, ce qu'une constante de version n'a aucune
 * raison de faire.
 */
export const EFFECTS_API_VERSION = 1

/**
 * What an effect declares, as the library lists it: its name is the file name,
 * the rest is what the module exports, read by Rust when it is compiled.
 *
 * `params` keeps the shape of `ParamSpec` as declared in TypeScript: the Rust
 * side does not interpret them, and typing them there would create a second
 * source of truth.
 */
export interface EffectManifest {
  name: string
  /** A string or a map of languages: see `localized` in `i18n/text.ts`. */
  description?: Text
  params?: Record<string, ParamSpec>
  apiVersion: number
  /** Declares `inputs: ['keys']`: key presses are read while it runs. */
  readsKeys?: boolean
}

/**
 * Writes one of the user's effects, `user:<name>`, to `<name>.ts` in the user's
 * folder, and returns its SHA-256, to hand back to {@link cacheEffect} with the
 * JavaScript compiled from it.
 *
 * `create` says which gesture this is: creating never overwrites an effect of
 * that name, in any case, and saving again never creates one. A built-in is
 * refused.
 */
export function saveEffectSource(key: string, source: string, create: boolean): Promise<string> {
  return invoke('save_effect_source', { key, source, create })
}

/**
 * Records the JavaScript compiled from the version of the file that has `hash`.
 *
 * Rust runs the module once to read what it declares and samples its swatch.
 * A module that does not load comes back `broken`, with its error; a file that
 * changed since `hash` is refused.
 */
export function cacheEffect(key: string, hash: string, js: string): Promise<EffectEntry> {
  return invoke('cache_effect', { key, hash, js })
}

/**
 * Renames one of the user's effects to the name `to`, moves its settings, and
 * returns its new key. A running effect keeps running under it.
 */
export function renameEffect(from: string, to: string): Promise<string> {
  return invoke('rename_effect', { from, to })
}

/**
 * What the effects were called before this run's migration, and the keys they
 * became, when it migrated some. Only for renaming the editor's drafts.
 */
export function legacyEffectIds(): Promise<Record<string, string>> {
  return invoke('legacy_effect_ids')
}

/** La source TypeScript d'un effet installé, pour la rouvrir dans l'éditeur. */
export function readEffectSource(id: string): Promise<string> {
  return invoke('read_effect_source', { id })
}

/**
 * Deletes one of the user's effects: its file, its cache, and the settings kept
 * for it on every device. There is no undo, so the caller asks first.
 *
 * A built-in is refused, as renaming and saving over one are: see
 * {@link restoreBuiltin}.
 *
 * Rust stops the loops running the effect, on every device, before erasing
 * anything: a loop left running would carry on with code whose file is gone.
 */
export function deleteEffect(id: string): Promise<void> {
  return invoke('delete_effect', { id })
}

/**
 * Copies an effect into the user's folder — under its own name when that folder
 * holds none, then `<name> (2)`, `(3)`… — and returns the copy's key. The copy
 * is ready at once, and it is the user's.
 */
export function duplicateEffect(id: string): Promise<string> {
  return invoke('duplicate_effect', { id })
}

/** The names of the shipped effects their folder no longer holds, to offer them back. */
export function missingBuiltins(): Promise<string[]> {
  return invoke('missing_builtins')
}

/**
 * Writes a shipped effect's file again, by name: a missing one comes back, a
 * modified one is overwritten, and it receives updates again.
 *
 * The application does not delete, rename or save over a built-in: an edited
 * copy would stop receiving updates, a renamed or deleted one would never come
 * back. Duplicating makes an editable copy, in the user's folder.
 */
export function restoreBuiltin(name: string): Promise<void> {
  return invoke('restore_builtin', { name })
}

/** Opens the user's effects folder in the system file manager. */
export function openEffectsDir(): Promise<void> {
  return invoke('open_effects_dir')
}

/**
 * Forgets the settings kept for an effect the folder no longer holds: its
 * parameters on every device, and where it was applied.
 */
export function forgetEffectSettings(id: string): Promise<void> {
  return invoke('forget_effect_settings', { id })
}

/**
 * Whether an effect can run, as far as its cache says.
 *
 * - `ready`: compiled for the file's current bytes;
 * - `stale`: never compiled, or changed since — see `refreshLibrary`;
 * - `broken`: compiled, and it does not load — `error` says why.
 */
export type EffectState = 'ready' | 'stale' | 'broken'

/**
 * A library effect: its manifest, plus what is not part of it.
 *
 * Every effect is a file; `builtin` marks one of the shipped folder, which the
 * application does not delete, rename or save over.
 */
export interface EffectEntry extends EffectManifest {
  /** The effect's key, `shipped:<name>` or `user:<name>`: see `effectKey.ts`. */
  id: string
  kind: 'builtin' | 'user'
  state: EffectState
  /** Why a `broken` effect does not load. */
  error?: string
  /** SHA-256 of the source file, for {@link cacheEffect}. */
  hash?: string
  /** A built-in whose file was edited outside the application. */
  modified: boolean
  /**
   * Repère de couleurs, **prélevé en exécutant l'effet** — jamais déclaré.
   *
   * Quelques couleurs `#rrggbb`, rendues à des instants différents et à des
   * endroits différents du clavier : un effet uniforme et un effet spatial ne
   * peuvent donc pas se ressembler. Le Rust les calcule à l'installation et les
   * range à côté du manifeste ; elles arrivent avec la liste, sans second appel.
   *
   * **Le tableau peut être vide** — un effet qui lève pendant l'échantillonnage
   * s'installe quand même. L'interface montre alors une pastille neutre.
   *
   * Sa longueur n'est pas garantie : le jour où la galerie voudra des vignettes
   * animées, ce sera le même champ, avec plus d'images.
   */
  swatch: string[]
}

/** Built-in effects and the files, in one list, in a stable order. */
export function listEffects(): Promise<EffectEntry[]> {
  return invoke('list_effects')
}

/**
 * Valeurs de départ d'un effet : le `default` de chaque paramètre déclaré.
 *
 * Ici, avec le manifeste qui les porte, et non dans l'éditeur : la galerie lance
 * désormais un effet, et elle n'a aucune raison de charger le module d'analyse
 * syntaxique pour recopier quatre valeurs par défaut.
 *
 * ⚠️ **La même règle est écrite en Rust**, dans `storage::starting_params` :
 * l'icône de zone de notification lance un effet sans fenêtre, elle ne peut donc
 * rien emprunter d'ici. Ce que les deux doivent dire pareil : les défauts du
 * manifeste, recouverts par ce qu'on a retenu (`merge`, dans
 * `useSettings`), et **bornés aux paramètres déclarés**. Les désaccorder
 * donnerait deux éclairages différents pour le même effet selon l'endroit d'où
 * on l'a lancé.
 */
export function startingParams(declaring: Pick<EffectManifest, 'params'>): EffectParams {
  const out: EffectParams = {}
  for (const [key, spec] of Object.entries(declaring.params ?? {})) out[key] = spec.default
  return out
}

// ---------------------------------------------------------------- réglages

export type EffectParams = Record<string, ParamValue>

/** Une décision d'adoption, telle que `settings.json` la retient. */
export interface DeviceRecord {
  vid: number
  pid: number
  /** Absent quand le système n'en déclare pas — pas `null`. */
  serial?: string
  state: DeviceState
  /**
   * Luminosité retenue pour **cet** appareil. Absente = {@link BRIGHTNESS_DEFAULT}.
   *
   * Ici et non à la racine : `set_brightness(device, level)` prend un `DeviceRef`
   * depuis le premier jour, et le protocole en fait une commande de l'appareil
   * distincte de l'effet en cours. Deux claviers n'ont aucune raison de partager
   * un niveau.
   */
  brightness?: number
}

/**
 * L'effet **appliqué** sur un appareil — celui qui pilote ses LED.
 *
 * Une liste indexée, pas un scalaire : le moteur fait tourner un effet par
 * appareil, et un champ unique ne pouvait pas décrire ça.
 *
 * **L'aperçu n'écrit jamais ici.** Regarder un effet ne le retient pas ; c'est
 * « Appliquer » qui décide.
 */
export interface ActiveEffectRecord {
  vid: number
  pid: number
  effect: string
}

/**
 * Ce qui vaut pour l'application entière, et pour aucun appareil en particulier.
 *
 * Un objet à part : tout ce qui dépend d'un clavier vit dans une liste indexée,
 * ce qui n'en dépend pas vit ici. C'est le rangement qui empêche la confusion
 * dont `settings.json` vient de sortir — et la langue, quand elle arrivera,
 * n'aura rien à arbitrer.
 */
export interface Preferences {
  /** Le niveau du journal, quand quelqu'un l'a changé. Absent = le défaut. */
  logLevel?: LogLevel
  /** The interface language someone chose. Absent = the system's. */
  language?: LanguageSetting
  /** A device that opens starts its applied effect again. Absent = on. */
  resumeEffects?: boolean
  /** Log files kept, one per day; `0` keeps them all. Absent = 7. */
  logFilesKept?: number
  /** Light or dark interface. Absent = the system's. */
  theme?: ThemeSetting
}

/** Turns resuming applied effects on or off. */
export function setResumeEffects(on: boolean): Promise<void> {
  return invoke('set_resume_effects', { on })
}

/** Whether Candeo launches at login, and whether this build can change it. */
export interface LaunchAtLogin {
  enabled: boolean
  /** False in a development build, and on a system Candeo writes no entry for. */
  available: boolean
}

export function getLaunchAtLogin(): Promise<LaunchAtLogin> {
  return invoke('get_launch_at_login')
}

/** Writes or removes the system's login entry, hidden in the notification area. */
export function setLaunchAtLogin(on: boolean): Promise<LaunchAtLogin> {
  return invoke('set_launch_at_login', { on })
}

/** What someone chose for the interface theme. */
export type ThemeSetting = 'system' | 'light' | 'dark'

/** Saves the interface theme; the window applies it itself. */
export function setTheme(theme: ThemeSetting): Promise<void> {
  return invoke('set_theme', { theme })
}

/** What someone chose for the interface language. */
export type LanguageSetting = 'system' | 'en' | 'fr'

/** A language the interface is written in. */
export type Language = 'en' | 'fr'

/** The setting, the language it resolves to, and the system's. */
export interface LanguageStatus {
  setting: LanguageSetting
  language: Language
  system: Language
}

/** The interface language; the system's when the settings cannot be read. */
export function getLanguage(): Promise<LanguageStatus> {
  return invoke('get_language')
}

/** Changes the interface language and saves it. */
export function setLanguage(setting: LanguageSetting): Promise<LanguageStatus> {
  return invoke('set_language', { setting })
}

/**
 * Les réglages retenus pour un effet, sur un appareil.
 *
 * `values` ne porte que ce qui **diffère** de ce que l'effet déclare : un
 * paramètre laissé à sa valeur de départ n'y figure pas, et suivra donc le
 * manifeste si une version ultérieure de l'effet en change le défaut.
 *
 * La clé est la paire appareil / effet, **sans numéro de série** : toutes les
 * commandes du moteur visent un `DeviceRef`, deux exemplaires du même modèle
 * partagent déjà leur boucle de rendu, et les distinguer ici promettrait une
 * séparation que le reste de l'application ne tient pas.
 */
export interface EffectParamsRecord {
  vid: number
  pid: number
  effect: string
  values: EffectParams
}

/**
 * Miroir de `Settings`, dans `src-tauri/src/storage.rs`.
 *
 * Une préférence globale dans `preferences`, tout ce qui dépend d'un clavier dans
 * une liste indexée. Chacune ne porte que ce qui **diffère du défaut** : un
 * appareil absent de `devices` est détecté et à pleine luminosité, et un appareil
 * absent d'`activeEffects` ne s'est vu appliquer aucun effet.
 */
export interface Settings {
  /** Shape of the file: 3 since every effect is referenced by its key, `<source>:<name>`. */
  version: number
  /**
   * The shipped effects copied into the folder: the hash of the version copied,
   * or `null` when a file of that name was already there. Kept when the file is
   * deleted or renamed, so that it is not copied again.
   */
  shippedEffects: Record<string, string | null>
  preferences: Preferences
  devices: DeviceRecord[]
  activeEffects: ActiveEffectRecord[]
  effectParams: EffectParamsRecord[]
}

/**
 * Lit `settings.json`.
 *
 * Au premier lancement il n'y a pas de fichier : ce sont les **défauts** qui
 * arrivent, ce n'est pas une erreur.
 */
export function getSettings(): Promise<Settings> {
  return invoke('get_settings')
}

/**
 * Remet `settings.json` au défaut, et repose les appareils.
 *
 * Ce qui part : les décisions d'adoption — tout repasse en `detected` —, la
 * luminosité retenue de chaque appareil, l'effet appliqué sur chacun, et les
 * réglages retenus par paire appareil / effet. Le Rust arrête d'abord les
 * boucles en cours, éteint le rétroéclairage et referme les appareils : remettre
 * la table des appareils à zéro pendant qu'un effet tourne laisserait des
 * boucles que plus aucune décision ne désigne.
 *
 * **Aucun effet n'est touché.** Les effets écrits vivent dans le dossier de
 * données, pas dans `settings.json` ; les retirer est une autre action, une par
 * effet ({@link deleteEffect}). Les confondre ferait perdre du code écrit à la
 * main à qui voulait seulement désadopter un clavier.
 *
 * La fenêtre garde, elle, ce qu'elle avait lu : c'est à l'appelant d'oublier les
 * réglages tenus en mémoire, sans quoi le premier mouvement de curseur les
 * réécrirait.
 */
export function resetSettings(): Promise<void> {
  return invoke('reset_settings')
}

/**
 * Retient les réglages d'un effet pour un appareil, sans toucher au reste.
 *
 * À ne pas confondre avec {@link setEffectParams}, qui ajuste la boucle en
 * cours : celle-ci écrit sur disque, et ne change rien à ce qui tourne. Les deux
 * n'ont ni la même cadence ni la même destination.
 *
 * Une commande dédiée plutôt qu'un `set_settings` : le Rust relit, modifie et
 * réécrit d'un seul tenant. Renvoyer tout le fichier depuis la fenêtre
 * écraserait au passage une adoption décidée entre-temps.
 *
 * Une table **vide** efface l'entrée : c'est « rétablir les valeurs déclarées ».
 */
export function rememberEffectParams(
  device: DeviceRef,
  effect: string,
  params: EffectParams,
): Promise<void> {
  return invoke('remember_effect_params', { device, effect, params })
}

// ---------------------------------------------------------------- moteur

export interface EngineStatus {
  running: boolean
  effectId: string | null
  /** Erreur venant du code de l'effet. Déjà lisible : à afficher telle quelle. */
  error: string | null
  /**
   * Échec d'écriture vers le clavier — sans rapport avec le code de l'effet.
   *
   * Les deux sont distincts parce qu'ils n'ont ni la même cause ni le même
   * remède : un effet impeccable peut n'atteindre aucune LED.
   */
  deviceError: Failure | null
  /**
   * Vrai si les images parviennent effectivement à un clavier.
   *
   * Faux avec la sortie coupée, mais aussi — et c'est le cas piégeux —
   * quand aucun périphérique n'est connecté : le simulateur s'anime, la case
   * « envoyer » reste cochée, et le clavier garde son image. On lit ça comme
   * « seule la première image est passée ».
   */
  reachingKeyboard: boolean
  toKeyboard: boolean
}

/** L'état d'un appareil, et à qui il appartient. */
export interface DeviceEngineStatus extends EngineStatus {
  device: DeviceRef
}

/**
 * Ce que la fenêtre **regarde**, et qui n'atteint aucun clavier.
 *
 * Un type distinct, dans un champ distinct : c'est la quatrième fois dans ce
 * projet qu'un état qui ment coûte une session de diagnostic, et un drapeau à
 * filtrer se filtre mal. Ici il n'y a rien à filtrer — l'aperçu n'est pas dans
 * la liste des appareils, et personne ne peut l'y trouver par mégarde.
 *
 * Il ne porte ni `toKeyboard`, ni `reachingKeyboard`, ni `deviceError` : une
 * boucle d'aperçu n'a aucune sortie matérielle, et ces champs à faux se liraient
 * comme une panne là où il n'y a qu'un choix.
 */
export interface PreviewStatus {
  /**
   * L'appareil dont l'aperçu **emprunte** le gabarit.
   *
   * Il n'est ni piloté ni forcément branché : c'est une géométrie, pas une
   * destination.
   */
  layoutOf: DeviceRef
  running: boolean
  effectId: string | null
  /** Erreur venant du code de l'effet. Déjà lisible : à afficher telle quelle. */
  error: string | null
}

/**
 * Tout ce que le moteur sait, **rangé de façon à ne pas se confondre**.
 *
 * `devices` décrit ce qui tourne sur le matériel — c'est ce que liste l'icône de
 * zone de notification. `preview` décrit ce qu'on regarde.
 */
export interface EngineReport {
  devices: DeviceEngineStatus[]
  /** `null` quand rien n'est prévisualisé — dont dès que la fenêtre est repliée. */
  preview: PreviewStatus | null
}

/**
 * Démarre un effet installé **sur un appareil**.
 *
 * Le moteur tourne dans un fil Rust par appareil, indépendant de la fenêtre :
 * fermer l'application n'éteint pas l'effet. Démarrer ici ne touche à aucun
 * autre appareil — chacun porte son effet et ses réglages.
 *
 * Viser un appareil débranché n'est pas une erreur : la boucle tourne, le
 * simulateur s'anime, et `reachingKeyboard` reste faux jusqu'à l'ouverture.
 * C'est ce qui permet d'écrire un effet **sans posséder le clavier**.
 */
export function startEffect(
  device: DeviceRef,
  id: string,
  params: EffectParams = {},
): Promise<void> {
  return invoke('start_effect', { device, id, params })
}

export function stopEffect(device: DeviceRef): Promise<void> {
  return invoke('stop_effect', { device })
}

/**
 * Démarre l'aperçu d'un effet, **sans toucher au clavier ni au disque**.
 *
 * Le pendant exact de {@link startEffect}, moins tout ce qui engage : aucune
 * sortie matérielle, rien d'écrit dans `settings.json`, et surtout **aucune
 * boucle d'appareil arrêtée**. C'est ce qui permet de parcourir la galerie
 * pendant qu'un effet tourne sur le clavier : sans cette boucle séparée,
 * sélectionner un effet éteindrait l'éclairage en cours.
 *
 * `device` désigne l'appareil dont on **emprunte le gabarit** ; `null` retombe
 * sur le gabarit par défaut, pour prévisualiser sans posséder de clavier.
 *
 * Il n'y a qu'un aperçu : appeler à nouveau **remplace** le précédent. Chaque
 * appel construit un contexte QuickJS et en détruit un, d'où la temporisation
 * côté appelant — la borner ici obligerait à choisir entre faire attendre la
 * dernière sélection et la perdre.
 */
export function startPreview(
  device: DeviceRef | null,
  id: string,
  params: EffectParams = {},
): Promise<void> {
  return invoke('start_preview', { device, id, params })
}

/** Arrête l'aperçu. Aucun effet d'appareil n'est touché. */
export function stopPreview(): Promise<void> {
  return invoke('stop_preview')
}

/** Ajuste les paramètres de l'aperçu à chaud, sans redémarrer sa boucle. */
export function setPreviewParams(params: EffectParams): Promise<void> {
  return invoke('set_preview_params', { params })
}

/**
 * Ouvre le flux d'images de l'aperçu vers le simulateur.
 *
 * Un canal distinct de celui des appareils, et c'est ce qui permet de regarder un
 * effet pendant qu'un autre tourne sur le clavier : les deux flux existent en
 * même temps, et la fenêtre choisit lequel elle dessine.
 */
export function subscribePreviewFrames(
  onFrame: (frame: Uint8Array) => void,
): Promise<() => void> {
  const channel = new Channel<ArrayBuffer | number[]>()
  channel.onmessage = (m) => {
    onFrame(m instanceof ArrayBuffer ? new Uint8Array(m) : Uint8Array.from(m))
  }
  return invoke<void>('subscribe_preview_frames', { channel }).then(
    () => () => void invoke('unsubscribe_preview_frames'),
  )
}

/** Ajuste les paramètres à chaud, sans redémarrer la boucle. */
export function setEffectParams(device: DeviceRef, params: EffectParams): Promise<void> {
  return invoke('set_effect_params', { device, params })
}

/**
 * Active ou coupe l'écriture vers un appareil, **sans** toucher au simulateur.
 *
 * C'est ce qui permet d'écrire un effet sans posséder le clavier. La coupure
 * est propre à l'appareil visé : les autres continuent d'être alimentés.
 */
export function setOutputToKeyboard(device: DeviceRef, on: boolean): Promise<void> {
  return invoke('set_output_to_keyboard', { device, on })
}

/**
 * Ouvre le flux d'images d'**un appareil** vers le simulateur.
 *
 * Chaque message est une image brute : `frameLen × 3` octets, dans l'ordre RVB.
 * Un canal, et non un événement global — la portée est explicite et le binaire
 * passe sans détour par un tableau JSON d'entiers.
 *
 * Le simulateur suit l'appareil sélectionné : en changer, c'est fermer ce canal
 * et s'abonner ailleurs. Fermer le canal arrête le flux **sans arrêter
 * l'effet**, qui continue d'alimenter le clavier.
 */
export function subscribeFrames(
  device: DeviceRef,
  onFrame: (frame: Uint8Array) => void,
): Promise<() => void> {
  const channel = new Channel<ArrayBuffer | number[]>()
  channel.onmessage = (m) => {
    onFrame(m instanceof ArrayBuffer ? new Uint8Array(m) : Uint8Array.from(m))
  }
  return invoke<void>('subscribe_frames', { device, channel }).then(
    () => () => void invoke('unsubscribe_frames', { device }),
  )
}

/**
 * État du moteur : ce qui tourne **sur les appareils**, et ce qu'on **regarde**.
 *
 * Interrogé plutôt que poussé : une erreur survenue fenêtre fermée doit se lire
 * à la réouverture, ce qu'un événement ponctuel ne permet pas.
 *
 * `devices` porte une entrée par appareil visé depuis le démarrage, pas seulement
 * par appareil ouvert : un appareil sans ligne est un appareil dont on ne sait
 * rien, ce qui n'est pas la même chose qu'un appareil qui ne fait rien.
 *
 * `preview` est à part — voir {@link PreviewStatus}.
 */
export function engineStatus(): Promise<EngineReport> {
  return invoke('engine_status')
}

// ------------------------------------------------------- changements hors fenêtre

/**
 * L'état a changé **sans la fenêtre**.
 *
 * Écrit ici *et* dans `src-tauri/src/tray.rs`, qui confronte les deux par un
 * test : rien ne relie ces deux chaînes à la compilation, et les désaccorder
 * donnerait une fenêtre qui ne se resynchronise plus jamais, sans une erreur
 * nulle part.
 */
const ETAT_CHANGE = 'candeo://etat-change'

/**
 * Prévient quand l'icône de zone de notification a commandé quelque chose, ou
 * quand la fenêtre revient après avoir été repliée.
 *
 * # Pourquoi un événement, ici, alors que tout le reste est interrogé
 *
 * Ce qui est **interrogeable** l'est resté : l'état du moteur se relit toutes
 * les secondes, précisément parce qu'une erreur survenue fenêtre fermée doit se
 * lire à la réouverture. Ce qui ne l'est pas, c'est ce que la fenêtre ne lit
 * qu'**une fois**, au montage — la liste des appareils, `settings.json` — parce
 * qu'elle en était jusqu'ici la seule source.
 *
 * Elle ne l'est plus, et surtout elle ne meurt plus : fermer la fenêtre la
 * replie, son instantané peut donc vieillir des jours pendant que le menu
 * commande les effets. Sonder le disque en boucle pour cela serait payer à
 * chaque seconde ce qui arrive quelques fois par session.
 *
 * Rend de quoi se désabonner, comme {@link subscribeFrames}.
 */
export function onStateChanged(handler: () => void): Promise<UnlistenFn> {
  return listen(ETAT_CHANGE, () => {
    handler()
  })
}

// ---------------------------------------------------------------- journal

/** Miroir de `LogLevel`, dans `src-tauri/src/journal.rs`. */
export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace'

/**
 * Les niveaux que la fenêtre sait produire.
 *
 * Sans `trace` : le par-image vient du moteur, pas d'ici, et un niveau qu'on ne
 * sait pas écrire n'a pas à être acceptable en argument.
 */
export type WebviewLevel = Exclude<LogLevel, 'trace'>

/** Miroir de `JournalStatus`, dans `src-tauri/src/journal.rs`. */
export interface JournalStatus {
  /**
   * Le niveau appliqué. `null` quand `CANDEO_LOG` porte une directive qu'aucun
   * niveau ne résume — à afficher tel quel plutôt qu'en inventer un.
   */
  level: LogLevel | null
  /** Le niveau retenu dans `settings.json`. `null` = le défaut. */
  setting: LogLevel | null
  /**
   * Vrai si `CANDEO_LOG` impose le niveau. Le réglage est alors **écrit mais pas
   * appliqué** : il vaudra au prochain lancement sans la variable.
   */
  forcedByEnv: boolean
  /** Le dossier des journaux, `null` si le journal n'écrit pas sur disque. */
  dir: string | null
  /**
   * Vrai si le niveau actif porte du **par-image**.
   *
   * ⚠️ C'est ce qui rend visible qu'un niveau élevé est actif : laissé en place
   * et oublié, il remplit le disque en silence.
   */
  verbose: boolean
  /** Log files kept, one per day; `0` keeps them all. */
  filesKept: number
}

/** L'état du journal : niveau appliqué, niveau retenu, dossier. */
export function getJournal(): Promise<JournalStatus> {
  return invoke('get_journal')
}

/**
 * Change le niveau **sans redémarrer**, et le retient.
 *
 * Le défaut qu'on cherche peut ne pas survivre au redémarrage — un clavier qui
 * décroche après deux heures, un appareil qui disparaît par intermittence :
 * « relancez en mode détaillé » revient à demander de reproduire ce qu'on vient
 * d'observer.
 *
 * Il **survit au redémarrage**, et c'est un choix : un défaut qui ne se produit
 * qu'au lancement existe. D'où `verbose`, qui sert à le dire.
 */
export function setLogLevel(level: LogLevel): Promise<JournalStatus> {
  return invoke('set_log_level', { level })
}

/** Changes how many log files are kept, and deletes those beyond it now. */
export function setLogFilesKept(keep: number): Promise<JournalStatus> {
  return invoke('set_log_files_kept', { keep })
}

/** Ouvre le dossier des journaux dans le gestionnaire de fichiers du système. */
export function openLogDir(): Promise<void> {
  return invoke('open_log_dir')
}

/**
 * Le diagnostic, prêt à coller dans un rapport de bogue : version, système,
 * appareils, état du moteur.
 *
 * Le numéro de série n'y figure pas — une empreinte stable le remplace, qui
 * distingue deux exemplaires du même modèle sans divulguer lequel.
 */
export function diagnostic(): Promise<string> {
  return invoke('diagnostic')
}

/**
 * Consigne un enregistrement venu de la fenêtre dans le journal du Rust.
 *
 * À n'appeler que par la façade de `api/journal.ts` : elle seule sait qu'un
 * échec de journalisation doit être avalé.
 */
export function logFromWebview(
  level: WebviewLevel,
  source: string,
  message: string,
): Promise<void> {
  return invoke('log_from_webview', { level, source, message })
}
