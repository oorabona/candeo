/**
 * API d'écriture d'effets.
 *
 * Un effet est une fonction pure du temps et de la position vers une couleur.
 * C'est ce qui rend YAML inadapté : on décrirait une configuration, pas un
 * comportement. Ici l'effet *est* du code.
 *
 * ## Le contrat
 *
 * Un module d'effet **exporte par défaut** un {@link EffectModule}. Le moteur
 * ne cherche rien d'autre :
 *
 * ```ts
 * import { hsv } from '@candeo/effects-api'
 *
 * export default {
 *   description: 'Mon effet',
 *   render({ layout, time, frame }) { … },
 * } satisfies EffectModule
 * ```
 *
 * ## Ce fichier a un jumeau
 *
 * ⚠️ Il décrit ce que l'**éditeur** montre en autocomplétion ; ce que le moteur
 * fournit réellement est écrit dans
 * `apps/desktop/src-tauri/src/runtime/api.js`. S'ils divergent, l'éditeur
 * promet une fonction qui n'existe pas, et l'erreur ne se voit qu'à la première
 * image. Le test Rust `api_js_exports_match_the_typescript_surface` échoue si
 * un nom disparaît du jumeau — toute modification doit toucher les deux.
 */

export interface Rgb {
  r: number
  g: number
  b: number
}

/**
 * A matrix position that carries an LED.
 *
 * **Two spaces live here, and they do not say the same thing.**
 *
 * - `row`/`col` place the LED in the **matrix**. Neighborhood has a meaning
 *   there, but a cell is a cell: the space bar takes **a single one** despite its
 *   6.25 u, and matrix holes count as distance although they take no space.
 * - `x`/`y`/`w`/`h` give the **physical rectangle** of the keycap, the one the
 *   simulator draws.
 *
 * An effect that talks about distance must therefore choose what it measures:
 * among the shipped effects, "Onde radiale" measures the keycaps, "Onde
 * diagonale" counts matrix steps.
 */
export interface Key {
  /** Index de LED dans l'image. */
  readonly index: number
  readonly row: number
  readonly col: number
  /**
   * The key's name in the keyboard layout the system uses — "Z" or "ECHAP" on a
   * French Windows. For display: it changes with the layout, so find keys by
   * {@link scancode}. Absent when the system gives no name.
   */
  readonly label?: string
  /**
   * What the keyboard sends for this key, in PS/2 set 1: the make code, `0xE0` in
   * the high byte for an extended key (`0xE01D`, right Ctrl). It names the
   * physical key whatever its legend: the keys under the left hand of a gamer are
   * `[0x11, 0x1E, 0x1F, 0x20]`, engraved ZQSD on AZERTY and WASD on QWERTY. Both
   * LEDs of the ISO Enter are `0x1C`.
   *
   * Absent for a key that sends nothing (Fn), and when the layout does not say.
   */
  readonly scancode?: number
  /**
   * Bord gauche du capuchon, en **unités de pas de clavier** : 1 u = la largeur
   * d'une touche alphabétique. L'origine est en haut à gauche, `y` croît vers le
   * bas, et la touche occupe `[x, x + w[ × [y, y + h[`.
   *
   * ## Pourquoi `u`, et pas une fraction du clavier
   *
   * Le pas est une grandeur **absolue** — 19,05 mm sur tout clavier pleine
   * taille. `1 u` désigne donc la même distance sur un pleine taille, un TKL ou
   * un pavé de macros, et une échelle réglée en `u` garde son sens d'un appareil
   * à l'autre : « un anneau toutes les six touches » reste un anneau toutes les
   * six touches. Normaliser sur l'encombrement ferait l'inverse — le même `0,5`
   * vaudrait onze touches ici et trois ailleurs, et l'effet changerait d'aspect
   * sans qu'une ligne change.
   *
   * Ce qu'un appareil plus petit change, c'est le **nombre** d'anneaux visibles,
   * pas leur taille. Ce qu'un effet ne doit pas supposer, en revanche, c'est où
   * est le centre : il se lit dans {@link bounds}, jamais dans `rows`/`cols`.
   *
   * C'est aussi l'unité que porte le Rust (`crates/candeo-device/src/layout.rs`)
   * : rien ne se convertit en chemin, donc rien ne peut s'y tromper.
   *
   * ## ⚠️ Facultatif, et c'est le fond du sujet
   *
   * La géométrie n'est pas une lecture du périphérique — il n'expose que sa
   * grille logique — mais une transcription faite à la main. Tous les gabarits
   * n'en ont pas : c'est la capacité `geometry` de `docs/design/device-sdk.md`
   * §3.2.
   *
   * Un effet qui en dépend doit donc le **dire en échouant**, jamais soustraire
   * `undefined` : la distance vaudrait `NaN`, la couleur serait bornée à zéro,
   * et le clavier resterait noir sans un mot. {@link center} et {@link bounds}
   * sont là pour ça — elles lèvent en nommant la touche qui n'a pas de
   * rectangle.
   */
  readonly x?: number
  /** Bord supérieur, en unités de pas. Voir {@link x}. */
  readonly y?: number
  /** Largeur, en unités de pas. Voir {@link x}. */
  readonly w?: number
  /** Hauteur, en unités de pas. Voir {@link x}. */
  readonly h?: number
}

export interface Layout {
  readonly name: string
  readonly rows: number
  readonly cols: number
  /** Uniquement les positions portant une LED. */
  readonly keys: readonly Key[]
}

export interface Frame {
  /** Écrit une couleur à une position. */
  set(key: Key, color: Rgb): void
  /** Écrit la même couleur partout. */
  fill(color: Rgb): void
}

export interface EffectContext<P = undefined> {
  readonly layout: Layout
  /**
   * Secondes écoulées depuis le démarrage de l'effet.
   *
   * **C'est l'horloge, et la seule.** Elle est prise sur le temps réel, pas
   * comptée en images : une image sautée ne ralentit donc pas l'animation, elle
   * l'échantillonne moins souvent. Animer sur `time` garde la même vitesse
   * quelle que soit la charge de la machine.
   */
  readonly time: number
  /**
   * Numéro d'image, incrémenté à chaque rendu.
   *
   * ⚠️ **Ce n'est pas une horloge.** La boucle vise 30 images par seconde mais
   * ne les garantit pas : une machine chargée en fait moins, et les images
   * manquées ne sont **pas** rattrapées. `frameIndex * 0.016` n'est donc pas
   * une durée, et un effet animé dessus **ralentit** au lieu de sauter — sans
   * rien signaler.
   *
   * Il sert à ce qui se compte en images et non en secondes : alterner une
   * image sur deux, semer un générateur pseudo-aléatoire, espacer un
   * rafraîchissement coûteux. Pour tout mouvement, c'est {@link time}.
   */
  readonly frameIndex: number
  readonly frame: Frame
  /**
   * Paramètres déclarés par l'effet, tels que réglés dans l'interface.
   *
   * `Rgb` fait partie de l'union parce qu'un {@link ParamSpec} de type `color`
   * a pour valeur une couleur, pas un nombre : l'omettre obligerait tout effet
   * paramétré par une couleur à passer par un transtypage, pour contourner une
   * déclaration fausse.
   */
  readonly params: ParamsOf<P>
  /**
   * The keys pressed recently on this layout, oldest first. Always empty unless
   * the effect declares `inputs: ['keys']`.
   *
   * A press is a key going down: holding a key does not repeat it, and releases
   * are not reported. Only presses younger than 10 seconds, at most the last 32.
   * On a device, only that keyboard's presses; in the preview, any keyboard's.
   */
  readonly presses: readonly Press[]
}

/** A key going down. See {@link EffectContext.presses}. */
export interface Press {
  /** The key, as found in `layout.keys` by scancode. */
  readonly key: Key
  /** When it went down, on the clock of {@link EffectContext.time}. */
  readonly at: number
}

/** What an effect reads besides time and its parameters. */
export type Input = 'keys'

/** Un effet rend une image à chaque appel. */
export type Effect = (ctx: EffectContext) => void

/**
 * Ce que vaut un paramètre réglé dans l'interface.
 *
 * Exactement l'ensemble des `default` que {@link ParamSpec} peut porter. Nommé
 * plutôt qu'écrit deux fois : l'éditeur s'en sert pour typer ce qu'il envoie à
 * `start_effect`, et les deux unions ne doivent pas pouvoir diverger.
 */
export type ParamValue = number | string | boolean | Rgb

/**
 * La valeur que porte un paramètre, déduite de sa déclaration.
 *
 * C'est ce qui évite d'écrire `params.couleur as Rgb` dans un effet — un
 * transtypage serait de toute façon impossible dans un effet intégré, qui est
 * du JavaScript exécuté tel quel par le moteur.
 */
type ValueOfSpec<S> = S extends { kind: 'number' }
  ? number
  : S extends { kind: 'color' }
    ? Rgb
    : S extends { kind: 'boolean' }
      ? boolean
      : S extends { kind: 'choice' }
        ? string
        : ParamValue

/**
 * Les paramètres tels que `render` les reçoit.
 *
 * Sans déclaration — un effet qui n'en a pas — on retombe sur la forme large,
 * ce qui laisse l'effet fonctionner sans rien déclarer.
 */
export type ParamsOf<P> = P extends Record<string, ParamSpec>
  ? { readonly [K in keyof P]: ValueOfSpec<P[K]> }
  : Readonly<Record<string, ParamValue>>

/**
 * Text shown to the user: a string, or the same text in several languages.
 *
 * ```ts
 * label: 'Vitesse'
 * label: { en: 'Speed', fr: 'Vitesse' }
 * ```
 *
 * The interface picks its own language, then English, then the first entry.
 * A plain string suits an effect written for one language.
 */
export type Text = string | { readonly [language: string]: string }

/**
 * An option of a `choice`: its value alone, shown as it is, or the value the effect
 * receives and the text shown for it.
 *
 * ```ts
 * options: ['calm', 'wild']
 * options: [{ value: 'calm', label: { en: 'Calm', fr: 'Calme' } }]
 * ```
 */
export type ChoiceOption = string | { readonly value: string; readonly label: Text }

/** Déclaration d'un paramètre réglable, pour que l'interface le présente. */
export type ParamSpec =
  | { kind: 'number'; label: Text; min: number; max: number; step?: number; default: number }
  | { kind: 'color'; label: Text; default: Rgb }
  | { kind: 'boolean'; label: Text; default: boolean }
  | { kind: 'choice'; label: Text; options: readonly ChoiceOption[]; default: string }

/** A kind of device an effect can target. The list grows with the devices. */
export type DeviceKind = 'keyboard'

export interface EffectModule<P = undefined> {
  /**
   * @deprecated The file name is the effect's name: this property is ignored.
   * Kept so that effects written before still type-check.
   */
  readonly name?: string
  /** What the effect does, in one sentence. See {@link Text}. */
  readonly description?: Text
  /**
   * The version of the effects API this effect was written against. Absent
   * means the first one; Candeo refuses to load an effect written for a newer
   * version than it knows.
   */
  readonly apiVersion?: number
  /**
   * The kinds of device this effect is meant for. Only keyboards exist today, so
   * an effect that says nothing is read as `['keyboard']`; saying it is what
   * keeps the effect right the day a second kind arrives.
   */
  readonly kinds?: readonly DeviceKind[] | 'all'
  /**
   * What the effect reads besides time and its parameters. `['keys']` gives it
   * {@link EffectContext.presses}; key presses are read only while such an
   * effect runs, and the gallery says so (`docs/design/key-input.md`).
   */
  readonly inputs?: readonly Input[]
  readonly params?: P
  /** `ctx.params` est typé d'après `params` ci-dessus. */
  readonly render: (ctx: EffectContext<P>) => void
}

/**
 * Déclare un effet.
 *
 * Ne fait **rien** à l'exécution — elle rend son argument tel quel. Son seul
 * rôle est de donner un type contextuel à l'objet littéral, ce qui type les
 * paramètres de `render` :
 *
 * ```ts
 * export default defineEffect({
 *   description: 'Mon effet',
 *   render({ layout, time, frame }) { … },   // typés, sans annotation
 * })
 * ```
 *
 * Sans cette enveloppe — ou sans `satisfies EffectModule` — un objet littéral
 * n'a aucun type contextuel : `layout`, `time`, `frame` et `params` sont alors
 * implicitement `any`, et `strict` les refuse. Quatre erreurs, sur la façon la
 * plus naturelle d'écrire un effet ; c'est précisément ce que cette fonction
 * évite.
 */
export function defineEffect<
  const P extends Readonly<Record<string, ParamSpec>> | undefined = undefined,
>(effect: EffectModule<P>): EffectModule<P> {
  return effect
}

// ---------------------------------------------------------------- utilitaires

export const rgb = (r: number, g: number, b: number): Rgb => ({
  r: clampByte(r),
  g: clampByte(g),
  b: clampByte(b),
})

export const BLACK: Rgb = { r: 0, g: 0, b: 0 }

function clampByte(v: number): number {
  return Math.max(0, Math.min(255, Math.round(v)))
}

/** Teinte 0-360, saturation et valeur 0-1. */
export function hsv(h: number, s: number, v: number): Rgb {
  const c = v * s
  const hp = (((h % 360) + 360) % 360) / 60
  const x = c * (1 - Math.abs((hp % 2) - 1))
  const [r, g, b] =
    hp < 1 ? [c, x, 0] :
    hp < 2 ? [x, c, 0] :
    hp < 3 ? [0, c, x] :
    hp < 4 ? [0, x, c] :
    hp < 5 ? [x, 0, c] :
             [c, 0, x]
  const m = v - c
  return rgb((r + m) * 255, (g + m) * 255, (b + m) * 255)
}

export const lerp = (a: number, b: number, t: number): number => a + (b - a) * t

export function mix(a: Rgb, b: Rgb, t: number): Rgb {
  return rgb(lerp(a.r, b.r, t), lerp(a.g, b.g, t), lerp(a.b, b.b, t))
}

// ------------------------------------------------------------------ géométrie
//
// Deux fonctions, et elles ont le même rôle : lire un rectangle **ou échouer en
// le disant**. C'est tout ce qui sépare un effet spatial portable d'un effet qui
// rend du noir sur les gabarits qu'on n'a pas sous la main. Voir {@link Key.x}.

/** Un rectangle en unités de pas de clavier. */
export interface Rect {
  readonly x: number
  readonly y: number
  readonly w: number
  readonly h: number
}

/**
 * Le centre du capuchon d'une touche, en unités de pas.
 *
 * Le centre et non le coin : c'est là qu'est la LED, et c'est ce qui place la
 * barre d'espace au milieu de ses 6,25 u plutôt qu'à son bord gauche.
 *
 * @throws si la touche n'a pas de rectangle — voir {@link Key.x}.
 */
export function center(key: Key): { x: number; y: number } {
  const { x, y, w, h } = key
  if (x === undefined || y === undefined || w === undefined || h === undefined) {
    throw new TypeError(sansRectangle(key))
  }
  return { x: x + w / 2, y: y + h / 2 }
}

/**
 * L'encombrement physique du dessin, en unités de pas.
 *
 * C'est ce qui remplace `rows`/`cols` dès qu'on parle de distance : le milieu du
 * clavier est en `x + w / 2`, et il y reste sur un gabarit sans pavé numérique
 * comme sur un pleine taille. `(cols - 1) / 2` désigne le milieu de la
 * **matrice**, qui n'est le milieu de rien de visible.
 *
 * Même définition que le `viewBox` du simulateur : le centre d'une onde est
 * donc le centre de ce qu'on regarde.
 *
 * Un gabarit sans aucune touche rend un rectangle nul — il n'y a rien à
 * encadrer, et rien non plus à éclairer.
 *
 * @throws dès qu'une touche n'a pas de rectangle — voir {@link Key.x}.
 */
export function bounds(layout: Layout): Rect {
  if (layout.keys.length === 0) return { x: 0, y: 0, w: 0, h: 0 }

  let x0 = Infinity
  let y0 = Infinity
  let x1 = -Infinity
  let y1 = -Infinity

  for (const key of layout.keys) {
    const { x, y, w, h } = key
    if (x === undefined || y === undefined || w === undefined || h === undefined) {
      throw new TypeError(sansRectangle(key))
    }
    if (x < x0) x0 = x
    if (y < y0) y0 = y
    if (x + w > x1) x1 = x + w
    if (y + h > y1) y1 = y + h
  }

  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

/**
 * La phrase que lisent {@link center} et {@link bounds} en échouant.
 *
 * Elle nomme la touche fautive : un gabarit contribué peut être dessiné à
 * moitié, et « il manque un rectangle » sans dire lequel ne se corrige pas.
 */
function sansRectangle(key: Key): string {
  const which = key.label === undefined ? `position ${key.index}` : `“${key.label}”`
  return (
    `${which} has no rectangle: this layout has no surveyed geometry, ` +
    'and an effect that measures physical distances has nothing to measure on it.'
  )
}

// L'exemple de référence vit dans `example.ts` : ce fichier décrit l'API, il
// n'exporte pas d'effet. Un export par défaut ici ferait de la bibliothèque
// elle-même un effet, ce qu'elle n'est pas.
//
// The effects shipped with the application live in `packages/effects/`: plain
// `.ts` files against this same API, copied into the effects folder at startup.
// They are the best reading of what can be written here.
