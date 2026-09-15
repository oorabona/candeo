<script lang="ts">
import { ref } from 'vue'

/**
 * L'état des deux colonnes repliables, au niveau du module.
 *
 * Passer à l'éditeur détruit cette vue : depuis `setup()`, la colonne qu'on
 * vient de replier se rouvrirait au retour, et la bande morte qu'on voulait
 * supprimer reviendrait à chaque aller-retour. Même motif que `useDevice` —
 * l'état d'écran survit à la navigation, il ne se sérialise pas pour autant.
 *
 * Rangé dans un objet, et repris nommément dans `<script setup>` : seules les
 * liaisons de ce bloc-là sont exposées au patron.
 */
const shut = { devices: ref(false), effects: ref(false) }
</script>

<script setup lang="ts">
/**
 * Studio — trois colonnes : appareils, effets, réglages.
 *
 * La hiérarchie est celle dans laquelle on pense : on choisit un appareil, puis
 * son effet, puis ses réglages. **L'affectation n'est plus une case à cocher en
 * bas de panneau, c'est la structure de l'écran.**
 *
 * ## Deux colonnes se replient, la troisième non
 *
 * Avec un seul appareil piloté, une colonne entière serait une bande morte
 * permanente, et c'est l'aperçu qui a besoin de la largeur. La troisième ne se
 * replie pas : c'est le contenu, il ne resterait rien. Le détail du repliement
 * est dans la feuille de style, là où le piège se trouve.
 *
 * ## Un seul effet « actif »
 *
 * Celui de l'appareil **sélectionné**, et lui seul. Marquer actifs les effets de
 * tous les appareils dans une liste qui décrit ce que fait *un* appareil n'est
 * pas une simplification, c'est une information fausse.
 *
 * ## Sélectionner lance l'aperçu, « Appliquer » envoie au clavier
 *
 * On réglait à l'aveugle puis on découvrait le résultat sur le clavier ; c'est
 * l'inverse désormais (issue #63). **Deux boucles, et elles ne se touchent
 * pas** : celle de l'appareil écrit sur les LED, celle de l'aperçu n'écrit nulle
 * part. Parcourir la galerie ne peut donc pas éteindre l'éclairage en cours — ce
 * qui serait arrivé avec une seule boucle par appareil, et ne se serait vu
 * qu'une fois livré.
 *
 * L'écran dit **lequel des deux** il montre, à chaque instant : `engine_status`
 * les range dans deux champs distincts, et il n'y a rien à filtrer ici.
 *
 * Le simulateur vient dans le panneau de droite : liste à gauche / rendu à
 * droite ici, code à gauche / rendu à droite dans l'éditeur. Même grammaire, et
 * **un seul dessin** — `KeyboardSimulator` est le même composant des deux côtés,
 * il n'y a pas deux tracés à tenir d'accord.
 *
 * Il est alimenté par un canal d'images du moteur, celles-là mêmes qui partent
 * vers le clavier quand c'est l'appareil qu'on regarde (`docs/design/studio.md`
 * §3).
 */

import { computed, nextTick, onBeforeUnmount, onMounted, watch } from 'vue'
import { useRouter } from 'vue-router'
import type { ParamSpec, ParamValue } from '@candeo/effects-api'

import {
  deleteEffect,
  duplicateEffect,
  forgetEffectSettings,
  missingBuiltins,
  openEffectsDir,
  restoreBuiltin,
  engineStatus,
  getDefaultLayout,
  getLayout,
  startEffect,
  stopEffect,
  type EffectEntry,
  type EffectState,
  type EngineReport,
} from '../api/candeo'
import { effectName as nameOfKey, isShippedKey } from '../api/effectKey'
import { message } from '../api/journal'
import type { DeviceRef } from '../api/types'
import EffectParamsForm from '../components/EffectParamsForm.vue'
import DeviceStatusDot from '../components/DeviceStatusDot.vue'
import EffectSwatch from '../components/EffectSwatch.vue'
import KeyboardSimulator from '../components/KeyboardSimulator.vue'
import { deviceStatus, statusLabel } from '../composables/deviceStatus'
import { deviceEffect } from '../composables/effectSelection'
import { useDevice } from '../composables/useDevice'
import { hardwareEffects, useEffects, type HardwareEffect } from '../composables/useEffects'
import { useSettings } from '../composables/useSettings'
import { refreshLibrary } from '../editor/library'
import { t } from '../i18n'
import { localized } from '../i18n/text'
import type { LayoutView } from '../keyboard/layout'
import { useSimulatorFeed } from '../keyboard/simulatorFeed'

/** Période d'interrogation du moteur, en millisecondes. */
const STATUS_PERIOD = 1000

/**
 * Plage de la luminosité.
 *
 * Un octet, parce que c'est ce que la trame porte (`0x0f`/`0x04`) — pas un choix
 * d'interface. À ne pas confondre avec `BRIGHTNESS_DEFAULT`, qui vaut la même
 * chose aujourd'hui pour une tout autre raison : le maximum est une contrainte du
 * protocole, le défaut est une décision.
 */
const BRIGHTNESS_MAX = 255

const shutDevices = shut.devices
const shutEffects = shut.effects

const router = useRouter()
const { devices, current, select, busy, refresh } = useDevice()
// `apply` ne lève pas : il range son échec dans `applyError`, qu'il faut donc
// afficher — sans quoi un mode matériel refusé par l'appareil ne dirait rien.
const { appliedOn, apply, error: applyError } = useEffects()
const {
  load: loadSettings,
  reload: reloadSettings,
  valuesFor,
  keptFor,
  adjust,
  adjustPreview,
  settle,
  forget,
  dropEffect,
  referencedEffects,
  lastAppliedOn,
  brightnessOf,
  setBrightness,
  flush: flushParams,
  error: paramsError,
} = useSettings()

/** Les erreurs remontées par Rust sont déjà lisibles : on les affiche telles quelles. */
// ---------------------------------------------------------------- appareils

/**
 * La colonne liste les appareils **pilotés**, et rien d'autre.
 *
 * L'adoption reste dans la vue Périphériques : choisir ce qu'on configure et
 * choisir ce que Candeo a le droit de piloter sont deux gestes différents, et
 * les fondre ferait d'un clic de sélection une prise de contrôle.
 */
const piloted = computed(() => devices.value.filter((d) => d.state === 'adopted'))

/**
 * L'appareil que la colonne montre comme choisi.
 *
 * Le choix courant de l'application s'il est piloté, sinon le premier de la
 * liste : `current` peut désigner un gabarit connu mais non adopté, qui n'a
 * aucune ligne ici.
 */
const selectedDevice = computed(() => {
  const list = piloted.value
  const c = current.value
  return list.find((d) => c !== null && d.vid === c.vid && d.pid === c.pid) ?? list[0] ?? null
})

/** Identité stable d'un appareil, pour comparer sans dépendre de l'objet. */
const key = (d: DeviceRef | null) => (d ? `${d.vid}:${d.pid}` : null)

const deviceKey = computed(() => key(selectedDevice.value))

/**
 * Ce que la colonne désigne devient le choix de toute l'application.
 *
 * C'est ce qui fait qu'« ouvrir dans l'éditeur » travaille sur l'appareil qu'on
 * regardait : l'éditeur n'a pas de colonne, il reprend `current`.
 */
watch(
  deviceKey,
  () => {
    const d = selectedDevice.value
    if (d && key(current.value) !== key(d)) select({ vid: d.vid, pid: d.pid })
  },
  { immediate: true },
)

function choose(d: { vid: number; pid: number }) {
  select({ vid: d.vid, pid: d.pid })
}

/**
 * L'avertissement attend la fin de la recherche lancée au démarrage. Sans ce
 * `busy`, il s'afficherait le temps de l'énumération puis disparaîtrait :
 * annoncer une absence qu'on n'a pas encore vérifiée.
 */
const noDevice = computed(() => piloted.value.length === 0 && !busy.value)

// ---------------------------------------------------------------- bibliothèque

type Nature = 'builtin' | 'user' | 'hardware'

/**
 * Un effet, quelle que soit sa nature.
 *
 * Les trois listes n'ont ni la même origine ni la même forme — `list_effects`
 * pour les deux premières, un catalogue écrit pour le matériel — mais la colonne
 * les affiche de la même façon. On les ramène donc à une seule forme ici plutôt
 * que de tenir trois gabarits de gabarit dans le patron.
 */
interface Choice {
  id: string
  name: string
  nature: Nature
  description: string
  /**
   * Repère de couleurs **prélevé en exécutant l'effet**, côté Rust (issue #29).
   *
   * Vide pour le matériel, et c'est la seule réponse honnête : ces effets sont
   * exécutés par le micrologiciel, l'application ne voit jamais leurs images.
   * `EffectSwatch` montre alors une pastille sourde — inventer quatre couleurs
   * plausibles serait décrire un effet qu'on n'a pas regardé.
   */
  swatch: string[]
  params: Record<string, ParamSpec>
  /** Renseigné pour la seule nature qui ne passe pas par le moteur. */
  hardware: HardwareEffect | null
  /**
   * Whether the effect can run. Only a file can be anything but `ready`: one not
   * compiled yet, or one that does not load.
   */
  state: EffectState
  /** Why a `broken` effect does not load. */
  error: string | null
  /** A built-in whose file was edited outside the application. */
  modified: boolean
  /** Key presses are read while it runs: said on screen (`docs/design/key-input.md` §3). */
  readsKeys: boolean
}

const library = ref<EffectEntry[]>([])
/** False until the library was read once: before that, every effect looks missing. */
const libraryRead = ref(false)
/** Déjà lisible : les messages du Rust s'affichent tels quels. */
const listError = ref<string | null>(null)
/** Ce qui a empêché d'appliquer ou d'arrêter. Déjà lisible aussi. */
const problem = ref<string | null>(null)

function fromEntry(e: EffectEntry): Choice {
  return {
    id: e.id,
    name: e.name,
    nature: e.kind,
    description: localized(e.description) || t('effects.noDescription'),
    swatch: e.swatch,
    params: e.params ?? {},
    hardware: null,
    state: e.state,
    error: e.error ?? null,
    modified: e.modified,
    readsKeys: e.readsKeys ?? false,
  }
}

function fromHardware(e: HardwareEffect): Choice {
  return {
    id: e.id,
    name: t(`effects.hardwareEffects.${e.key}.name`),
    nature: 'hardware',
    description: t(`effects.hardwareEffects.${e.key}.summary`),
    swatch: [],
    params: {},
    hardware: e,
    state: 'ready',
    error: null,
    modified: false,
    readsKeys: false,
  }
}

const choices = computed<Choice[]>(() => [
  ...library.value.filter((e) => e.kind === 'builtin').map(fromEntry),
  ...library.value.filter((e) => e.kind === 'user').map(fromEntry),
  ...hardwareEffects.map(fromHardware),
])

/**
 * Grouped by nature. The cost of each one is spelled out in the right-hand
 * panel, not merely suggested by the order.
 *
 * Hardware comes first: it costs no processor time and survives everything,
 * which often makes it the right choice (`docs/design/studio.md` §1).
 *
 * The order is for display only. When the selected device has no effect to
 * select (`followDevice`), the fallback still takes the first entry of
 * `choices`, a built-in: a hardware effect would open the screen on a simulator
 * with nothing to animate.
 */
const GROUPS: readonly Nature[] = ['hardware', 'builtin', 'user']

const grouped = computed(() =>
  GROUPS.map((nature) => ({
    nature,
    title: t(`effects.groups.${nature}`),
    items: choices.value.filter((c) => c.nature === nature),
  })),
)

/**
 * Folded sections, remembered per viewer in the webview storage.
 *
 * A viewer who never uses hardware effects folds them once; asking again at
 * every launch would make folding pointless. Storage may be refused (hardened
 * webview, read-only profile): sections then open expanded and still fold for
 * the session.
 */
const SECTION_STORAGE_PREFIX = 'candeo:effects-section:'

function readSectionFolded(nature: Nature): boolean {
  try {
    return localStorage.getItem(SECTION_STORAGE_PREFIX + nature) === 'folded'
  } catch {
    return false
  }
}

function writeSectionFolded(nature: Nature, folded: boolean): void {
  try {
    // Removed rather than stored as "expanded": expanded is the default, and a
    // missing key must keep meaning it.
    if (folded) localStorage.setItem(SECTION_STORAGE_PREFIX + nature, 'folded')
    else localStorage.removeItem(SECTION_STORAGE_PREFIX + nature)
  } catch {
    // See `SECTION_STORAGE_PREFIX`: the fold simply does not outlive the session.
  }
}

const foldedSections = ref<Record<Nature, boolean>>({
  hardware: readSectionFolded('hardware'),
  builtin: readSectionFolded('builtin'),
  user: readSectionFolded('user'),
})

/**
 * Folding only hides entries: the selection is left alone, so the settings
 * panel keeps showing the effect even when its section is folded.
 */
function toggleSection(nature: Nature): void {
  const folded = !foldedSections.value[nature]
  foldedSections.value[nature] = folded
  writeSectionFolded(nature, folded)
}

/** The real cost, spelled out: it is what tells the three natures apart. */
function cost(nature: Nature): string {
  return t(nature === 'hardware' ? 'effects.costs.hardware' : 'effects.costs.host')
}

const chosenEffect = ref<string | null>(null)

/**
 * False until the library and the engine status were read once. Selecting
 * before would show the first effect, and start its preview, for the time it
 * takes to learn which one the device runs.
 */
const loaded = ref(false)

const selectedEffect = computed<Choice | null>(() =>
  loaded.value
    ? (choices.value.find((c) => c.id === chosenEffect.value) ?? choices.value[0] ?? null)
    : null,
)

/** The effects column's body, to bring the selected entry into view. */
const effectsList = ref<HTMLElement | null>(null)

/**
 * Selects the selected device's effect: the one running on it, else the one it
 * remembers (#118).
 *
 * Only when the screen opens and when another device is chosen, never on the
 * engine refresh: an effect clicked since stays selected.
 */
async function followDevice(): Promise<void> {
  const d = selectedDevice.value
  chosenEffect.value = deviceEffect(
    runningOn(d),
    lastAppliedOn(d),
    choices.value.map((c) => c.id),
  )
  const c = selectedEffect.value
  if (!c) return
  // Unfolded for this visit only: the fold the viewer saved stays theirs.
  foldedSections.value[c.nature] = false
  await nextTick()
  effectsList.value
    ?.querySelector(`[data-effect="${CSS.escape(c.id)}"]`)
    ?.scrollIntoView({ block: 'nearest' })
}

watch(deviceKey, () => {
  if (loaded.value) void followDevice()
})

// ---------------------------------------------------------------- moteur

/**
 * L'état du moteur : ce qui tourne sur les appareils, et ce qu'on regarde.
 *
 * Tout est relu d'un coup : c'est un seul aller-retour par seconde, et la
 * colonne des appareils a besoin de chaque ligne pour dire ce que chacun fait
 * tourner.
 *
 * **Les deux champs ne se mélangent jamais.** `devices` décrit le matériel ;
 * `preview` ce que le simulateur montre quand l'effet sélectionné n'est pas
 * celui qui tourne. Tout ce qui parle d'« actif » dans cet écran lit le premier.
 */
const report = ref<EngineReport>({ devices: [], preview: null })

function statusOf(d: { vid: number; pid: number } | null) {
  if (!d) return null
  return report.value.devices.find((s) => s.device.vid === d.vid && s.device.pid === d.pid) ?? null
}

/**
 * L'effet qu'un appareil fait tourner — **sur ses LED**.
 *
 * La boucle hôte l'emporte sur le mode matériel : tant qu'elle pousse des
 * images, c'est elle qu'on voit sur les LED, quel que soit le mode posé avant.
 * L'aperçu n'entre pas dans ce calcul, et ne le peut pas : il n'est pas dans
 * `devices`.
 */
function runningOn(d: { vid: number; pid: number } | null): string | null {
  const s = statusOf(d)
  if (s?.running === true && s.effectId !== null) return s.effectId
  return appliedOn(d ? { vid: d.vid, pid: d.pid } : null)
}

function effectName(id: string | null): string | null {
  if (id === null) return null
  return choices.value.find((c) => c.id === id)?.name ?? nameOfKey(id)
}

/**
 * Ce que fait un appareil, en une ligne, pour la colonne de gauche.
 *
 * Un appareil au repos qui **se souvient** de son dernier effet le dit : c'est
 * là que se voit le fait que la configuration est enregistrée, à l'endroit même
 * où elle décrit quelque chose.
 */
function deviceLine(d: { vid: number; pid: number }): string {
  const tourne = effectName(runningOn(d))
  if (tourne !== null) return tourne
  const retenu = effectName(lastAppliedOn({ vid: d.vid, pid: d.pid }))
  return retenu !== null ? t('effects.deviceStopped', { name: retenu }) : t('effects.deviceIdle')
}

/** Le seul effet marqué **appliqué** : celui de l'appareil sélectionné. */
const activeId = computed(() => runningOn(selectedDevice.value))

const status = computed(() => statusOf(selectedDevice.value))
const runningHere = computed(() => status.value?.running === true)

/** L'aperçu en cours, quand il montre bien l'effet sélectionné. */
const preview = computed(() => {
  const p = report.value.preview
  if (!p || !p.running) return null
  return p.effectId === selectedEffect.value?.id ? p : null
})

/**
 * L'échec d'un aperçu, **quand il concerne l'effet qu'on regarde**.
 *
 * Le filtre sur l'identifiant n'est pas une précaution de style : l'état du
 * moteur est relu chaque seconde, et l'aperçu d'un effet cassé survit à la
 * sélection suivante le temps que le nouveau démarre. Sans lui, on attribuerait
 * à un effet la panne d'un autre — la façon la plus rapide de faire chercher au
 * mauvais endroit.
 */
const previewError = computed<string | null>(() => {
  const p = report.value.preview
  if (!p || p.running || p.effectId !== selectedEffect.value?.id) return null
  return p.error
})

async function refreshStatus(): Promise<void> {
  try {
    report.value = await engineStatus()
  } catch (e) {
    problem.value = message(e)
  }
}

// ---------------------------------------------------------------- aperçu

/**
 * Gabarit de repli, demandé au Rust : il faut bien dessiner quelque chose avant
 * qu'un appareil soit ouvert.
 */
const fallback = ref<LayoutView | null>(null)
/** Gabarit de l'appareil sélectionné, quand il est réellement ouvert. */
const opened = ref<LayoutView | null>(null)
const board = computed<LayoutView | null>(() => opened.value ?? fallback.value)

/**
 * Le dessin suit l'appareil sélectionné.
 *
 * Un seul gabarit est connu aujourd'hui, mais il vient de l'appareil et non
 * d'une constante : `get_layout` pour celui qui est ouvert, le gabarit par
 * défaut sinon. Le jour où un second modèle arrive, cette vue n'a rien à
 * apprendre.
 */
watch(
  [deviceKey, () => selectedDevice.value?.open === true],
  async ([, ouvert]) => {
    const d = selectedDevice.value
    if (!d || !ouvert) {
      opened.value = null
      return
    }
    // Un gabarit qu'on n'obtient pas n'est pas une panne : le repli dessine.
    opened.value = await getLayout({ vid: d.vid, pid: d.pid }).catch(() => null)
  },
  { immediate: true },
)

/**
 * L'effet sélectionné est-il déjà celui qui tourne **sur l'appareil** ?
 *
 * C'est la question qui décide de tout ce qui suit : dans ce cas le simulateur
 * montre les vraies images du clavier, et il n'y a aucune raison d'entretenir un
 * second contexte QuickJS pour afficher la même chose.
 */
const applied = computed(
  () => selectedEffect.value !== null && activeId.value === selectedEffect.value.id,
)

/** Vrai quand le simulateur doit afficher le flux de l'appareil. */
const showsDevice = computed(() => runningHere.value && applied.value)

const { frame, restartPreview } = useSimulatorFeed({
  layout: () => board.value,
  device: () => selectedDevice.value,
  showsDevice: () => showsDevice.value,
  // A hardware effect is run by the firmware and the app never sees its frames:
  // previewing it would mean inventing them.
  previewed: () => {
    const c = selectedEffect.value
    // Nor an effect that cannot run: its JavaScript is not there, or does not load.
    return c && !c.hardware && c.state === 'ready' ? c.id : null
  },
  params: () => paramValues.value,
  onError: (e) => {
    problem.value = message(e)
  },
})

/**
 * Ce que le simulateur montre, dit en toutes lettres plutôt que deviné.
 *
 * **C'est ici que se joue le refus du mensonge.** Un aperçu qui ressemble à une
 * application coûte une session de diagnostic — c'est la quatrième fois que ce
 * motif se présente dans ce projet. La phrase dit donc les deux choses à la
 * fois : ce qu'on regarde, et ce que le clavier fait pendant ce temps.
 */
const previewNote = computed(() => {
  const c = selectedEffect.value
  if (showsDevice.value) return t('effects.preview.device')
  if (c?.hardware) return t('effects.preview.hardware')

  const tourne = effectName(activeId.value)
  const ailleurs = runningHere.value && tourne !== null
  if (preview.value) {
    // Sans appareil piloté, ne pas promettre « Appliquer » : le bouton est
    // désactivé, et l'annoncer enverrait chercher pourquoi il ne répond pas.
    if (!selectedDevice.value) return t('effects.preview.noDevice')
    return ailleurs
      ? t('effects.preview.elsewhere', { name: tourne })
      : t('effects.preview.apply')
  }
  if (previewError.value !== null) return t('effects.preview.stopped')
  return t('effects.preview.starting')
})

// ---------------------------------------------------------------- actions

const working = ref(false)

/**
 * « Appliquer » **promeut l'aperçu en effet d'appareil** : c'est le geste qui
 * envoie au clavier, et le seul.
 *
 * Deux chemins, parce que les deux natures ne passent pas par le même endroit :
 * un effet matériel est un mode posé sur le micrologiciel, un effet hôte est une
 * boucle qu'on démarre. Poser un mode matériel arrête d'abord la boucle : sans
 * cela elle continuerait d'écrire par-dessus, et le mode resterait invisible.
 *
 * Rien n'arrête l'aperçu ici : `showsDevice` devient vrai dès la relecture de
 * l'état, la surveillance plus haut le range d'elle-même, et le simulateur passe
 * sur les images réelles. Le faire à la main en ferait deux chemins à tenir
 * d'accord.
 */
async function applyEffect(): Promise<void> {
  const c = selectedEffect.value
  const d = selectedDevice.value
  if (!c || !d) return

  const device = { vid: d.vid, pid: d.pid }
  problem.value = null
  working.value = true
  try {
    if (c.hardware) {
      if (statusOf(d)?.running === true) await stopEffect(device)
      await apply(device, c.hardware)
    } else {
      // Les réglages retenus pour **cette paire**, et non les valeurs déclarées :
      // un effet réglé puis quitté doit repartir comme on l'avait laissé, sans
      // quoi il faudrait rebouger chaque curseur après chaque « Appliquer ».
      //
      // Rien à abonner ici : le canal vit dans l'état de la boucle, et c'est la
      // surveillance plus haut qui l'ouvre dès que le moteur dit « en cours ».
      // Un abonnement de plus, posé ici, en ferait deux pour un seul flux.
      await startEffect(device, c.id, paramValues.value)
    }
  } catch (e) {
    problem.value = message(e)
  } finally {
    working.value = false
    await refreshStatus()
    // Le Rust vient de retenir — ou non — l'effet appliqué : relire est la seule
    // façon honnête de le savoir. Le deviner ici ferait de la fenêtre une
    // seconde source de vérité, qui divergerait au premier échec d'écriture.
    await reloadSettings()
  }
}

/**
 * Arrête la boucle de l'**appareil**. La dernière image reste affichée comme
 * elle reste sur le clavier : arrêter un effet n'éteint pas les LED.
 *
 * L'aperçu n'est pas concerné — et c'est bien le sujet : arrêter ce qui tourne
 * sur le clavier ne doit pas fermer ce qu'on est en train de regarder.
 */
async function halt(): Promise<void> {
  const d = selectedDevice.value
  if (!d) return

  problem.value = null
  working.value = true
  try {
    await stopEffect({ vid: d.vid, pid: d.pid })
  } catch (e) {
    problem.value = message(e)
  } finally {
    working.value = false
    await refreshStatus()
    // L'arrêt a **oublié** l'effet appliqué dans le fichier : sans cette
    // relecture, la colonne annoncerait encore « retenu : … » pour un appareil
    // dont plus rien n'est retenu.
    await reloadSettings()
  }
}

// ------------------------------------------------------------- suppression

/**
 * Only the user's effects are deleted from here. A built-in is not
 * (`docs/design/effects-library.md` §4), and a hardware effect lives in the
 * firmware.
 */
const removable = computed(() => selectedEffect.value?.nature === 'user')

/**
 * Shipped effects the folder no longer holds, offered back. Read with the
 * library, since both change with the folder.
 */
const missingBuiltinNames = ref<string[]>([])

async function readLibrary(): Promise<void> {
  const [entries, missing] = await Promise.all([refreshLibrary(), missingBuiltins()])
  library.value = entries
  missingBuiltinNames.value = missing
}

/**
 * Writes shipped effects' files again: missing ones, or a modified one whose
 * original is wanted back. The previewed code may have changed with them.
 */
async function restore(names: readonly string[]): Promise<void> {
  problem.value = null
  working.value = true
  try {
    for (const name of names) await restoreBuiltin(name)
  } catch (e) {
    problem.value = message(e)
  }
  try {
    await readLibrary()
    restartPreview()
  } catch (e) {
    listError.value = message(e)
  } finally {
    working.value = false
  }
}

/** The built-in whose original waits for confirmation, by id, like a removal. */
const pendingRestore = ref<string | null>(null)

async function restoreOriginal(): Promise<void> {
  const id = pendingRestore.value
  if (id === null) return
  await restore([nameOfKey(id)])
  if (problem.value === null) pendingRestore.value = null
}

/**
 * Effects the settings still refer to that the folder no longer holds: renamed
 * or removed outside the application.
 *
 * Said, not purged: putting the file back under its name restores everything,
 * and forgetting is a gesture of its own.
 */
const missingEffects = computed(() => {
  if (!libraryRead.value) return []
  const present = new Set(library.value.map((e) => e.id))
  return [...referencedEffects.value].filter((name) => !present.has(name)).sort()
})

async function forgetMissing(name: string): Promise<void> {
  problem.value = null
  try {
    await forgetEffectSettings(name)
    dropEffect(name)
  } catch (e) {
    problem.value = message(e)
  }
}

/** Copies the selected effect, then selects the copy. */
async function duplicateSelected(): Promise<void> {
  const c = selectedEffect.value
  if (!c || c.hardware) return
  problem.value = null
  working.value = true
  try {
    const name = await duplicateEffect(c.id)
    await readLibrary()
    chosenEffect.value = name
  } catch (e) {
    problem.value = message(e)
  } finally {
    working.value = false
  }
}

async function openFolder(): Promise<void> {
  try {
    await openEffectsDir()
  } catch (e) {
    listError.value = message(e)
  }
}

/**
 * Reads the effects folder again, compiling what changed: effects saved there
 * from outside the application appear, edited ones are recompiled.
 */
async function refreshEffects(): Promise<void> {
  listError.value = null
  working.value = true
  try {
    await readLibrary()
  } catch (e) {
    listError.value = message(e)
  } finally {
    working.value = false
  }
}

/**
 * L'effet dont la suppression attend confirmation, **par son identifiant**.
 *
 * Un identifiant et non un booléen : la question ne fige pas l'écran, on peut
 * cliquer ailleurs pendant qu'elle est posée, et un drapeau se retrouverait à
 * confirmer la suppression d'un autre effet que celui qu'on avait désigné.
 */
const pendingRemoval = ref<string | null>(null)

/**
 * Changer d'effet retire la question.
 *
 * Une confirmation qui survivrait à la sélection se rouvrirait d'elle-même au
 * retour, sans qu'on l'ait redemandée — et ce n'est pas une boîte qu'on veut
 * voir apparaître par surprise.
 */
watch(
  () => selectedEffect.value?.id,
  () => {
    pendingRemoval.value = null
    pendingRestore.value = null
  },
)

/**
 * Supprime l'effet désigné **par la confirmation**, jamais celui que la
 * sélection montre au moment du clic.
 *
 * Le Rust fait le reste dans l'ordre qu'il faut : il refuse ce qui n'est pas
 * supprimable, arrête les boucles qui font tourner cet effet sur quelque appareil
 * que ce soit, efface le dossier, puis oublie les réglages retenus pour lui. Rien
 * de tout cela n'est réparti ici — c'est la seule façon que l'invariant tienne
 * quel que soit l'appelant.
 */
async function removeEffect(): Promise<void> {
  const id = pendingRemoval.value
  if (id === null) return

  problem.value = null
  working.value = true
  try {
    await deleteEffect(id)

    // Le pendant en mémoire de ce que le Rust vient de faire sur disque : sans
    // cet oubli, un effet réenregistré sous le même nom dans la même session
    // hériterait des réglages de son homonyme disparu.
    dropEffect(id)

    pendingRemoval.value = null
    // La sélection retombe sur le premier de la liste : l'effet qu'elle désignait
    // n'existe plus.
    if (chosenEffect.value === id) chosenEffect.value = null
    await readLibrary()
  } catch (e) {
    problem.value = message(e)
  } finally {
    working.value = false
    // La boucle a pu s'arrêter : l'état du moteur ne le dira qu'une fois relu.
    await refreshStatus()
  }
}

// ---------------------------------------------------------------- réglages

/** Les paramètres déclarés par l'effet regardé. */
const specs = computed<Record<string, ParamSpec>>(() => selectedEffect.value?.params ?? {})

/**
 * Les valeurs sur lesquelles cet effet tourne — ou tournerait — sur cet
 * appareil : son manifeste, recouvert par ce qu'on a retenu pour cette paire.
 */
const paramValues = computed(() =>
  valuesFor(selectedDevice.value, selectedEffect.value?.id ?? '', specs.value),
)

/**
 * Pourquoi les contrôles sont inertes, ou `null` s'ils sont vivants.
 *
 * **Ils sont vivants presque toujours, désormais.** Ils l'étaient au seul effet
 * appliqué, ce qui obligeait à régler à l'aveugle puis à découvrir le résultat
 * sur le clavier ; la boucle d'aperçu supprime ce marché (issue #63) — on ajuste
 * en voyant, et sans rien envoyer nulle part.
 *
 * Reste le cas où il n'y a aucune boucle à ajuster : un effet matériel, dont le
 * micrologiciel n'expose rien, et le court instant où l'aperçu n'a pas encore
 * démarré.
 */
const frozen = computed<string | null>(() => {
  if (preview.value || showsDevice.value) return null
  // **Pas pendant que l'aperçu démarre.** Les contrôles restent vivants : ce
  // qu'on règle est retenu, et l'aperçu démarrera avec ces valeurs-là — c'est
  // `paramValues` qu'on lui passe. Les figer le temps d'un aller-retour ferait
  // clignoter le formulaire à chaque changement de sélection, pour rien.
  //
  // Un effet matériel n'a rien à ajuster non plus, mais il ne déclare aucun
  // paramètre : c'est `noParams` qui parle pour lui, et le redire ici ferait lire
  // deux fois la même phrase.
  return previewError.value === null ? null : t('effects.frozen')
})

/** Un effet sans paramètre le dit — et il ne le dit pas de la même façon selon sa nature. */
const noParams = computed(() =>
  selectedEffect.value?.hardware ? t('effects.noParamsHardware') : t('effects.noParams'),
)

/**
 * Ce que la configuration retient, dit à l'endroit où on la fabrique.
 *
 * C'est le défaut réel que l'issue #64 relève : les réglages sont conservés
 * depuis l'issue #28, et **rien à l'écran ne le laissait deviner**. On règle, on
 * ferme, et on n'a aucune raison de croire que ça a tenu.
 */
const savedNote = computed<string | null>(() => {
  const c = selectedEffect.value
  const d = selectedDevice.value
  if (!c || c.hardware || Object.keys(specs.value).length === 0) return null
  if (!d) return t('effects.savedNoDevice')
  return keptFor({ vid: d.vid, pid: d.pid }, c.id)
    ? t('effects.savedKept', { device: d.name })
    : t('effects.savedDeclared', { device: d.name })
})

/**
 * Un réglage part vers **la boucle qu'on regarde**, et sur disque.
 *
 * ⚠️ **Vers le clavier uniquement si c'est cet effet-là qui y tourne.** Une
 * première version poussait systématiquement vers les deux boucles, en pensant
 * qu'on réglait l'effet appliqué tout en en prévisualisant un autre. C'est faux :
 * `selectedEffect` est celui qu'on **regarde**, pas celui qui est appliqué.
 * Régler la couleur d'un effet prévisualisé changeait donc l'éclairage en cours,
 * et pouvait arrêter l'effet appliqué — les valeurs d'un effet arrivaient dans
 * la boucle d'un autre.
 *
 * Le disque, lui, retient toujours : le réglage appartient à la paire
 * appareil/effet et vaudra au prochain lancement de cet effet.
 */
function onParamChange(id: string, value: ParamValue): void {
  const d = selectedDevice.value
  const c = selectedEffect.value
  if (!d || !c) return
  const complete = adjust(
    { vid: d.vid, pid: d.pid },
    c.id,
    specs.value,
    id,
    value,
    c.id === activeId.value,
  )
  if (preview.value) adjustPreview(complete)
}

/** Le geste est fini — curseur relâché, case cochée : on écrit maintenant. */
function onParamCommit(): void {
  const d = selectedDevice.value
  const c = selectedEffect.value
  if (!d || !c) return
  settle({ vid: d.vid, pid: d.pid }, c.id)
}

function onParamReset(): void {
  const d = selectedDevice.value
  const c = selectedEffect.value
  if (!d || !c) return
  const declarees = forget({ vid: d.vid, pid: d.pid }, c.id, specs.value, c.id === activeId.value)
  if (preview.value) adjustPreview(declarees)
}

// ---------------------------------------------------------------- luminosité

/**
 * Le niveau de l'appareil sélectionné, de 0 à 255.
 *
 * Dans la colonne des périphériques, et non dans les réglages d'un effet : c'est
 * une propriété de l'appareil — une commande distincte du protocole (`0x0f`/
 * `0x04`), qui n'a rien à voir avec l'effet en cours. La commande existait depuis
 * le premier jour et n'était affichée nulle part : ce n'était pas un bogue
 * d'affichage, c'était une interface qui n'avait jamais été écrite (issue #64).
 */
const brightness = computed(() => brightnessOf(selectedDevice.value))

/** En pourcentage, parce que 0-255 ne veut rien dire pour qui règle sa lumière. */
const brightnessPercent = computed(() =>
  Math.round((brightness.value / BRIGHTNESS_MAX) * 100),
)

/**
 * `commit` sépare le glissement de sa fin : pendant, on écrit sur le clavier ;
 * à la fin seulement, sur le disque. Même partage que pour les réglages d'effet,
 * et pour la même raison — `settings.json` s'écrit par fichier temporaire puis
 * renommage, c'est un geste disque complet.
 */
function onBrightness(event: Event, commit: boolean): void {
  const d = selectedDevice.value
  if (!d) return
  const level = Number((event.target as HTMLInputElement).value)
  setBrightness({ vid: d.vid, pid: d.pid }, level, commit)
}

// ---------------------------------------------------------------- cycle de vie

let statusTimer = 0
/** Faux dès la destruction : l'ouverture enchaîne des allers-retours au Rust. */
let alive = true

onMounted(async () => {
  void getDefaultLayout().then((l) => {
    fallback.value = l
  })

  // Avant tout le reste : « Appliquer » et l'aperçu partent des valeurs
  // retenues, et les lire après coup laisserait une fenêtre où l'effet
  // démarrerait sur ses défauts.
  await loadSettings()

  await refresh()

  // Une seule alerte : la bibliothèque est lue d'un coup, elle échoue d'un coup.
  // Les effets matériels, eux, sont écrits ici : la colonne n'est jamais vide.
  try {
    await readLibrary()
    libraryRead.value = true
  } catch (e) {
    listError.value = message(e)
  }

  await refreshStatus()

  loaded.value = true
  void followDevice()

  // On a pu quitter l'écran entre-temps : poser l'interrogation périodique
  // maintenant la laisserait tourner pour personne.
  if (alive) statusTimer = window.setInterval(() => void refreshStatus(), STATUS_PERIOD)
})

onBeforeUnmount(() => {
  alive = false
  window.clearInterval(statusTimer)
  // Le dernier mouvement d'un curseur ne doit pas dépendre du fait qu'on soit
  // resté devant le temps du repos d'écriture.
  flushParams()
})
</script>

<template>
  <section class="studio" :class="{ 'shut-1': shutDevices, 'shut-2': shutEffects }">
    <!-- ------------------------------------------------------- appareils -->
    <section class="col devices" :class="{ shut: shutDevices }" :aria-label="t('effects.columns.devices')">
      <!--
        Un intitulé, pas un titre de niveau : le seul `h1` de l'écran est le nom
        de l'effet qu'on configure, et il vient après dans le document. Chaque
        colonne est déjà nommée pour les lecteurs d'écran par son `aria-label`.
      -->
      <div class="col-head">
        <p class="col-title">{{ t('effects.columns.devices') }}</p>
        <button
          class="collapse"
          type="button"
          aria-controls="col-devices"
          :aria-expanded="!shutDevices"
          :aria-label="shutDevices ? t('effects.expandDevices') : t('effects.collapseDevices')"
          @click="shutDevices = !shutDevices"
        >
          {{ shutDevices ? '›' : '‹' }}
        </button>
      </div>

      <div id="col-devices" class="col-body">
        <!--
          The card is a plain container, not the button: the brightness slider
          lives in it, and an interactive control nested in a button is invalid
          markup that would also re-select the device on every drag.
        -->
        <div
          v-for="d in piloted"
          :key="`${d.vid}:${d.pid}`"
          class="card"
          :class="{ selected: deviceKey === key(d) }"
        >
          <button
            class="entry"
            type="button"
            :aria-pressed="deviceKey === key(d)"
            :aria-label="`${d.name} · ${statusLabel(deviceStatus(d))}`"
            :title="`${d.name} · ${statusLabel(deviceStatus(d))}`"
            @click="choose(d)"
          >
            <!--
              Un pictogramme de type, pas un logo de fabricant : ce sont des
              marques protégées, elles ne distinguent pas un clavier d'une souris,
              et le nom du produit porte déjà l'information. Le seul gabarit connu
              est un clavier ; le jour où le Rust déclarera un type, il viendra de
              là plutôt que d'être deviné sur le nom.
            -->
            <!--
              The state rides on the pictogram rather than in the text: it stays
              visible when the column is collapsed to icons, and it says which
              device dropped without a line of its own.
            -->
            <span class="glyph">
              <svg
                viewBox="0 0 24 16"
                width="18"
                height="12"
                aria-hidden="true"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
              >
                <rect x="1" y="2" width="22" height="12" rx="2" />
                <path d="M6 11h12" stroke-linecap="round" />
                <path d="M5 6h1M9 6h1M13 6h1M17 6h1" stroke-linecap="round" />
              </svg>
              <DeviceStatusDot class="state-badge" :device="d" aria-hidden="true" />
            </span>

            <span class="entry-text">
              <!-- Le nom du **produit**, pas une catégorie : c'est ce qui
                   distingue deux claviers de la même marque. Il passe à la ligne
                   plutôt que d'être tronqué. -->
              <span class="dev-name">{{ d.name }}</span>
              <span class="dev-fx">{{ deviceLine(d) }}</span>
            </span>
          </button>

          <!--
            Brightness is a device property, not an effect setting, so it sits
            in the device card. Only the selected card carries it: one slider
            per row would weigh down the column for a setting set once.

            Collapsed column: the card rules below restore the button alone, so
            the slider is hidden without having to be named.
          -->
          <div v-if="deviceKey === key(d)" class="lum">
            <label class="lum-head" :for="`lum-${key(d)}`">
              <span class="lum-label">{{ t('effects.brightness') }}</span>
              <span class="lum-value">{{ t('effects.percent', { n: brightnessPercent }) }}</span>
            </label>
            <input
              :id="`lum-${key(d)}`"
              type="range"
              min="0"
              :max="BRIGHTNESS_MAX"
              step="1"
              :value="brightness"
              :disabled="!d.open"
              @input="onBrightness($event, false)"
              @change="onBrightness($event, true)"
            />
          </div>
        </div>

        <p v-if="!piloted.length" class="none">
          {{ t('effects.noDevice') }}
          <RouterLink to="/devices" class="link">{{ t('effects.chooseDevice') }}</RouterLink>
        </p>
      </div>
    </section>

    <!-- ---------------------------------------------------------- effets -->
    <section class="col effects" :class="{ shut: shutEffects }" :aria-label="t('effects.columns.effects')">
      <div class="col-head">
        <p class="col-title">{{ t('effects.columns.effects') }}</p>
        <button
          class="collapse"
          type="button"
          aria-controls="col-effects"
          :aria-expanded="!shutEffects"
          :aria-label="shutEffects ? t('effects.expandEffects') : t('effects.collapseEffects')"
          @click="shutEffects = !shutEffects"
        >
          {{ shutEffects ? '›' : '‹' }}
        </button>
      </div>

      <div id="col-effects" ref="effectsList" class="col-body">
        <template v-for="g in grouped" :key="g.nature">
          <!--
            The title attribute names the button once the column is collapsed
            and only the chevron is left.
          -->
          <button
            :id="`fx-section-head-${g.nature}`"
            class="group"
            type="button"
            :aria-expanded="!foldedSections[g.nature]"
            :aria-controls="`fx-section-${g.nature}`"
            :title="g.title"
            @click="toggleSection(g.nature)"
          >
            <svg
              class="chevron"
              viewBox="0 0 10 10"
              width="10"
              height="10"
              aria-hidden="true"
              fill="none"
              stroke="currentColor"
              stroke-width="1.6"
            >
              <path d="M3.5 2l3 3-3 3" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
            <span class="group-title">{{ g.title }}</span>
          </button>
          <!--
            `v-show` rather than `v-if`: the element named by `aria-controls`
            must exist while folded, and `display: none` already takes its
            entries out of the tab order.
          -->
          <div
            v-show="!foldedSections[g.nature]"
            :id="`fx-section-${g.nature}`"
            class="group-items"
            role="group"
            :aria-labelledby="`fx-section-head-${g.nature}`"
          >
            <!--
              « appliqué », et non « actif ». Le mot d'avant valait pour les deux
              états à la fois, or ils n'ont rien à voir : l'un dit ce que le
              clavier fait, l'autre ce qu'on regarde. La sélection, elle, se lit
              déjà sur `aria-pressed` et sur la bordure.
            -->
            <button
              v-for="c in g.items"
              :key="c.id"
              class="entry"
              type="button"
              :data-effect="c.id"
              :aria-pressed="selectedEffect?.id === c.id"
              :aria-label="activeId === c.id ? t('effects.appliedOnDevice', { name: c.name }) : c.name"
              :title="c.name"
              @click="chosenEffect = c.id"
            >
              <EffectSwatch class="mark" :colors="c.swatch" />
              <span class="fx-name">{{ c.name }}</span>
              <span v-if="activeId === c.id" class="fx-state">{{ t('effects.entryApplied') }}</span>
              <span v-else-if="c.state === 'broken'" class="fx-state broken">
                {{ t('effects.entryBroken') }}
              </span>
              <span v-else-if="c.state === 'stale'" class="fx-state stale">
                {{ t('effects.entryStale') }}
              </span>
            </button>
          </div>
        </template>

        <button class="new" type="button" @click="router.push({ name: 'editor' })">
          <span class="plus" aria-hidden="true">＋</span>
          <span>{{ t('effects.new') }}</span>
        </button>
        <button
          class="new"
          type="button"
          :disabled="working"
          :title="t('effects.refreshTitle')"
          @click="refreshEffects"
        >
          <span class="plus" aria-hidden="true">↻</span>
          <span>{{ t('effects.refresh') }}</span>
        </button>
        <button class="new" type="button" :title="t('effects.openFolderTitle')" @click="openFolder">
          <span class="plus" aria-hidden="true">↗</span>
          <span>{{ t('effects.openFolder') }}</span>
        </button>
        <!--
          With the column's other actions rather than in the Built-in heading,
          which is the folding button: this one stays reachable folded, and in
          the collapsed column.
        -->
        <button
          v-if="missingBuiltinNames.length"
          class="new"
          type="button"
          :disabled="working"
          :title="t('effects.restoreTitle', { names: missingBuiltinNames.join(', ') })"
          @click="restore(missingBuiltinNames)"
        >
          <span class="plus" aria-hidden="true">↺</span>
          <span>{{ t('effects.restoreBuiltins', { n: missingBuiltinNames.length }) }}</span>
        </button>
      </div>
    </section>

    <!-- --------------------------------------------------------- réglages -->
    <section class="col detail" :aria-label="t('effects.columns.settings')">
      <p v-if="listError" class="failure" role="alert">{{ listError }}</p>
      <p v-if="problem" class="failure" role="alert">{{ problem }}</p>
      <p v-if="applyError" class="failure" role="alert">{{ applyError }}</p>
      <p v-if="paramsError" class="failure" role="alert">{{ paramsError }}</p>
      <p v-if="status?.error" class="failure" role="alert">
        {{ t('effects.effectError', { error: status.error }) }}
      </p>
      <p v-if="status?.deviceError" class="notice warn" role="alert">
        {{ message(status.deviceError) }}
      </p>
      <!--
        L'erreur de l'aperçu est distincte de celle de l'effet appliqué, et le
        dit : un effet qu'on regarde peut lever pendant qu'un autre éclaire le
        clavier sans faute. Les confondre enverrait chercher au mauvais endroit.
      -->
      <p v-if="previewError" class="notice warn" role="alert">
        {{ t('effects.previewError', { error: previewError }) }}
      </p>

      <!--
        La bibliothèque se parcourt sans appareil : on doit pouvoir voir ce que
        l'application propose avant d'autoriser quoi que ce soit. L'aperçu, lui,
        tourne quand même — sur le gabarit par défaut, sans rien écrire nulle
        part. Mais l'écran dit ce qui manque et où aller.
      -->
      <p v-for="key in missingEffects" :key="key" class="notice warn" role="status">
        {{ t('effects.missing', { name: nameOfKey(key) }) }}
        <button
          v-if="isShippedKey(key) && missingBuiltinNames.includes(nameOfKey(key))"
          class="link"
          type="button"
          :disabled="working"
          @click="restore([nameOfKey(key)])"
        >
          {{ t('effects.restore') }}
        </button>
        <button class="link" type="button" @click="forgetMissing(key)">
          {{ t('effects.forgetSettings') }}
        </button>
      </p>

      <p v-if="noDevice" class="notice" role="status">
        {{ t('effects.noDeviceNotice') }}
        <RouterLink to="/devices" class="link">{{ t('effects.chooseDevice') }}</RouterLink>
      </p>

      <template v-if="selectedEffect">
        <header class="fx-head">
          <h1>{{ selectedEffect.name }}</h1>
          <span class="badge" :class="selectedEffect.nature">
            {{
              selectedEffect.modified
                ? t('effects.modified', { nature: t(`effects.natures.${selectedEffect.nature}`) })
                : t(`effects.natures.${selectedEffect.nature}`)
            }}
          </span>
          <span v-if="selectedEffect.readsKeys" class="badge keys">{{ t('effects.readsKeys') }}</span>
        </header>

        <p class="desc">{{ selectedEffect.description }}</p>
        <p v-if="selectedEffect.state === 'broken'" class="failure" role="alert">
          {{ selectedEffect.error }}
        </p>
        <p class="cost">{{ cost(selectedEffect.nature) }}</p>

        <div class="preview">
          <p class="cost">{{ previewNote }}</p>
          <!--
            Le gabarit vient du Rust : il est nul le temps d'un aller-retour. On
            ne dessine pas un clavier vide en attendant.
          -->
          <KeyboardSimulator v-if="board" class="sim" :layout="board" :frame="frame" />
          <p v-if="board && !opened" class="cost">{{ t('effects.preview.defaultLayout') }}</p>
        </div>

        <!--
          Les réglages, engendrés depuis le manifeste. Le formulaire ne connaît
          aucun effet en particulier : il connaît les quatre sortes de
          `ParamSpec`, et rien d'autre.
        -->
        <EffectParamsForm
          :specs="specs"
          :values="paramValues"
          :frozen="frozen"
          :empty="noParams"
          @change="onParamChange"
          @commit="onParamCommit"
          @reset="onParamReset"
        />

        <!-- Ce qui est retenu, dit là où on le fabrique. Voir `savedNote`. -->
        <p v-if="savedNote" class="cost">{{ savedNote }}</p>

        <footer class="actions">
          <button
            class="solid"
            :disabled="!selectedDevice || working || applied || selectedEffect.state !== 'ready'"
            @click="applyEffect"
          >
            {{ applied ? t('effects.applied') : t('effects.apply') }}
          </button>

          <!--
            Arrête la boucle de **l'appareil**, pas celle de l'effet sélectionné :
            c'est elle qui écrit, quel que soit l'effet qu'on regarde.
          -->
          <button v-if="runningHere" class="ghost" :disabled="working" @click="halt">
            {{ t('effects.stop') }}
          </button>

          <!-- Un effet matériel n'a pas de code : le dire vaut mieux que de
               laisser cliquer dans le vide. -->
          <button
            class="ghost"
            :disabled="selectedEffect.hardware !== null"
            @click="router.push({ name: 'editor', params: { id: selectedEffect.id } })"
          >
            {{ selectedEffect.nature === 'builtin' ? t('effects.viewCode') : t('effects.edit') }}
          </button>

          <button
            v-if="selectedEffect.hardware === null"
            class="ghost"
            :disabled="working"
            @click="duplicateSelected"
          >
            {{ t('effects.duplicate') }}
          </button>

          <button
            v-if="selectedEffect.modified"
            class="ghost"
            :disabled="working"
            @click="pendingRestore = selectedEffect.id"
          >
            {{ t('effects.restoreOriginal') }}
          </button>

          <!--
            Offered for the user's effects only.

            Il reste en place et actif pendant que la question est posée : le
            masquer ou le désactiver retirerait le focus du clavier au moment
            précis où il doit atteindre la réponse, qui suit dans le document.
          -->
          <button
            v-if="removable"
            class="ghost danger"
            :disabled="working"
            @click="pendingRemoval = selectedEffect.id"
          >
            {{ t('effects.delete') }}
          </button>

          <span class="spacer" />

          <p class="cost">
            {{ selectedEffect.hardware ? t('effects.footerHardware') : t('effects.footerPreview') }}
          </p>
        </footer>

        <!--
          La question est posée dans la colonne, pas dans une boîte modale : elle
          reste à côté de ce qu'elle décrit, et n'empêche pas de regarder ailleurs
          pendant qu'on y réfléchit.

          **Après** le bouton qui la déclenche, et c'est ce qui fait tout le
          parcours au clavier : la réponse est la tabulation suivante. Le titre
          porte `role="alert"` — l'encart apparaît sans que rien ne bouge à
          l'écran, il faut bien l'annoncer.
        -->
        <div
          v-if="pendingRemoval"
          class="confirm"
          role="group"
          aria-labelledby="confirm-remove"
        >
          <p id="confirm-remove" class="confirm-title" role="alert">
            {{ t('effects.deleteTitle', { name: selectedEffect.name }) }}
          </p>
          <!--
            Ce qui part, dit en toutes lettres. La source est le seul élément
            irremplaçable de la liste : les réglages se refont, la boucle se
            relance, le code écrit à la main ne se réinstalle pas.
          -->
          <p class="cost">{{ t('effects.deleteDetail') }}</p>
          <div class="confirm-actions">
            <button class="solid danger" :disabled="working" @click="removeEffect">
              {{ t('effects.deleteConfirm') }}
            </button>
            <button class="ghost" :disabled="working" @click="pendingRemoval = null">
              {{ t('effects.cancel') }}
            </button>
          </div>
        </div>

        <!-- Same placement and keyboard path as the removal question above. -->
        <div
          v-if="pendingRestore"
          class="confirm"
          role="group"
          aria-labelledby="confirm-restore"
        >
          <p id="confirm-restore" class="confirm-title" role="alert">
            {{ t('effects.restoreOriginalTitle', { name: selectedEffect.name }) }}
          </p>
          <p class="cost">{{ t('effects.restoreOriginalDetail') }}</p>
          <div class="confirm-actions">
            <button class="solid danger" :disabled="working" @click="restoreOriginal">
              {{ t('effects.restoreOriginal') }}
            </button>
            <button class="ghost" :disabled="working" @click="pendingRestore = null">
              {{ t('effects.cancel') }}
            </button>
          </div>
        </div>
      </template>
    </section>
  </section>
</template>

<style scoped>
/*
 * Les deux largeurs repliables sont des **variables**, pas des règles
 * concurrentes : `.shut-1` et `.shut-2` écrivent chacune la sienne, et le point
 * de rupture redéfinit `grid-template-columns` une bonne fois. Aucune des trois
 * n'a à l'emporter sur les autres — il n'y a rien à départager.
 */
.studio {
  --col-devices: 212px;
  --col-effects: 244px;

  display: grid;
  grid-template-columns: var(--col-devices) var(--col-effects) minmax(0, 1fr);
  height: 100%;
  min-height: 0;
}

.studio.shut-1 {
  --col-devices: 40px;
}

.studio.shut-2 {
  --col-effects: 40px;
}

.col {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  background: var(--raised);
  border-right: 1px solid var(--line);
}

.col-head {
  display: flex;
  gap: var(--gap-2);
  align-items: center;
  padding: var(--gap-2) var(--gap-2) var(--gap-2) var(--gap-3);
  border-bottom: 1px solid var(--line);
}

.col-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  color: var(--text-faint);
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  white-space: nowrap;
}

.collapse {
  flex: none;
  width: 22px;
  height: 22px;
  border: 1px solid transparent;
  border-radius: var(--r-sm);
  color: var(--text-faint);
  font-size: 13px;
  line-height: 1;
}

.collapse:hover {
  color: var(--accent);
  background: var(--raised-2);
  border-color: var(--line);
}

/* Conteneur de défilement : rien ne peut déborder latéralement d'une colonne
   repliée, quelle que soit l'erreur commise plus bas. */
.col-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: var(--gap-2) var(--gap-1) var(--gap-3);
}

.entry {
  display: flex;
  gap: var(--gap-2);
  align-items: center;
  width: 100%;
  padding: 6px var(--gap-2);
  border: 1px solid transparent;
  border-radius: var(--r-md);
  text-align: left;
}

.entry:hover {
  background: var(--raised-2);
}

/*
 * A device is framed by its card, not its button: the card also holds the
 * brightness slider, and framing the button alone would leave the slider
 * outside the selection it belongs to.
 */
.card {
  border: 1px solid transparent;
  border-radius: var(--r-md);
}

/* Doublé de la marque « actif » pour les effets, et de la position dans la
   liste pour les appareils : la bordure ambrée ne porte rien seule. */
.effects .entry[aria-pressed="true"],
.card.selected {
  background: var(--raised-2);
  border-color: var(--accent);
}

.glyph {
  position: relative;
  display: inline-flex;
  flex: none;
  color: var(--text-muted);
}

/* On the pictogram's lower right corner. Not `.badge`, which is the kind label
   of the effect panel. */
.state-badge {
  position: absolute;
  right: -4px;
  bottom: -3px;
}

.entry[aria-pressed="true"] .glyph {
  color: var(--accent);
}

.entry-text {
  display: block;
  min-width: 0;
}

.dev-name {
  display: block;
  font-weight: 500;

  /* Un nom de produit est long : il passe à la ligne plutôt que d'être
     tronqué — c'est lui qui distingue deux claviers de la même marque.
     `anywhere` couvre le cas d'une référence d'un seul tenant. */
  overflow-wrap: anywhere;
}

.dev-fx {
  display: block;
  overflow: hidden;
  color: var(--accent);
  font-size: 11px;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/*
 * Aligned with the device name, not the card edge, so the slider reads as part
 * of that device. The left offset adds up the button's border, padding, glyph
 * width and gap.
 */
.lum {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 0 var(--gap-2) 6px calc(1px + var(--gap-2) + 18px + var(--gap-2));
}

.lum-head {
  display: flex;
  flex-wrap: wrap;
  gap: 0 var(--gap-2);
  align-items: baseline;
  color: var(--text-faint);
  font-size: 11px;
}

.lum-label {
  flex: 1;
}

/* Le chiffre en clair : un curseur sans valeur ne se repose pas au même endroit
   d'une session à l'autre, et c'est justement ce qu'on retient ici. */
.lum-value {
  color: var(--text-muted);
  font-variant-numeric: tabular-nums;
}

.lum input {
  width: 100%;
  accent-color: var(--accent);
}

.lum input:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

.group {
  display: flex;
  gap: var(--gap-1);
  align-items: center;
  width: 100%;
  margin: var(--gap-3) 0 var(--gap-1);
  padding: 2px var(--gap-2);
  border-radius: var(--r-sm);
  color: var(--text-faint);
  font-size: 10px;
  letter-spacing: 0.08em;
  text-align: left;
  text-transform: uppercase;
}

.group:hover {
  color: var(--text-muted);
}

.group:first-child {
  margin-top: 0;
}

.chevron {
  flex: none;
  transition: transform 120ms ease;
}

/* Keyed on `aria-expanded` itself, so the chevron cannot disagree with what
   assistive technology announces. */
.group[aria-expanded="true"] .chevron {
  transform: rotate(90deg);
}

.fx-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.fx-state {
  flex: none;
  color: var(--accent);
  font-size: 10px;
  letter-spacing: 0.06em;
  text-transform: uppercase;
}

.fx-state.broken {
  color: var(--bad);
}

.fx-state.stale {
  color: var(--text-faint);
}

/* Le repère est décoratif — il porte déjà `aria-hidden`. Le rendre transparent
   au pointeur laisse l'infobulle du bouton passer : une fois la colonne
   repliée, c'est le seul endroit où le nom de l'effet se lit encore. */
.mark {
  pointer-events: none;
}

.new {
  width: 100%;
  margin-top: var(--gap-3);
  padding: 8px;
  border: 1px dashed var(--line-strong);
  border-radius: var(--r-md);
  color: var(--text-muted);
  font-size: 12px;
}

.new:hover:not(:disabled) {
  color: var(--accent);
  border-color: var(--accent);
}

.new + .new {
  margin-top: var(--gap-2);
}

.plus {
  margin-right: var(--gap-1);
}

/*
 * ---------------------------------------------------------------- repliement
 *
 * Deux fois la même règle, au même endroit : **on masque tous les enfants, puis
 * on rétablit explicitement le seul qui reste**.
 *
 * Ce n'est pas un détail de style. Énumérer ce qu'on cache — « cacher le nom,
 * cacher l'effet » — a déjà produit ici une collision de spécificité : une règle
 * ajoutée ailleurs pour le nom l'emportait sur le masquage, et le texte revenait
 * déborder dans 40 px. Écrite ainsi, la règle survit à l'ajout d'un enfant ou
 * d'une classe :
 *
 * 1. le sélecteur universel couvre ce qui n'existe pas encore — un enfant ajouté
 *    demain est masqué sans que personne ait à y penser ;
 * 2. le rétablissement est plus spécifique que le masquage, et il est ici, à
 *    deux lignes de lui, pas dans un autre bloc ;
 * 3. toute règle qui pourrait les concurrencer est gardée par `:not(.shut)` :
 *    elle ne **s'applique pas** en état replié, au lieu de gagner ou perdre un
 *    arbitrage de spécificité ;
 * 4. `.col-body` est un conteneur de défilement : même une règle fautive ne
 *    pourrait pas faire déborder la colonne sur sa voisine.
 */
@media (width > 820px) {
  .col.shut .col-head {
    justify-content: center;
    padding-inline: var(--gap-1);
  }

  .col.shut .col-title {
    display: none;
  }

  .col.shut .col-body {
    padding-inline: var(--gap-1);
  }

  /* First level: the body shows only device cards, section headers with their
     entries, and the add button. Everything else — messages, hints, whatever
     gets added — disappears without having to be named. */
  .col.shut .col-body > * {
    display: none;
  }

  .col.shut .col-body > .card {
    display: block;
  }

  /* Inside a card only the selection button survives: the brightness slider
     has no room in 40 px and goes with the rest. */
  .col.shut .card > * {
    display: none;
  }

  .col.shut .card > .entry {
    display: flex;
  }

  .col.shut .col-body > .group {
    display: flex;
  }

  /* A folded section keeps its own `display: none`, set inline by `v-show`,
     which this rule cannot override. */
  .col.shut .col-body > .group-items {
    display: block;
  }

  .col.shut .col-body > .new {
    display: block;
  }

  /* Second niveau : dans une entrée, un seul enfant survit — l'icône d'appareil
     ou le repère de couleurs. */
  .col.shut .entry > * {
    display: none;
  }

  .col.shut .entry > .glyph {
    display: block;
  }

  .col.shut .entry > .mark {
    display: flex;

    /* 34 px ne tiennent pas dans les 32 px utiles d'une colonne repliée : le
       repère se resserre plutôt que de faire défiler la colonne en largeur. */
    width: 26px;
  }

  .col.shut .entry {
    justify-content: center;
    align-items: center;
    gap: 0;
    padding-inline: 0;
  }

  /* Même forme pour le bouton d'ajout : tout masqué, le signe rétabli. */
  .col.shut .new > * {
    display: none;
  }

  .col.shut .new > .plus {
    display: inline;
    margin-right: 0;
  }

  .col.shut .new {
    padding-inline: 0;
  }

  /*
   * A section header shrinks to a rule and its chevron, same form as above:
   * everything hidden, the chevron restored. It stays a button, because a
   * header reduced to a bare rule would leave a folded section impossible to
   * reopen without first expanding the column.
   */
  .col.shut .group > * {
    display: none;
  }

  .col.shut .group > .chevron {
    display: block;
  }

  .col.shut .group {
    justify-content: center;
    margin: var(--gap-2) 0 var(--gap-1);
    padding: 2px 0;
    border-top: 1px solid var(--line);
    border-radius: 0;
  }

  .col.shut .group:first-child {
    margin-top: 0;
    border-top: none;
  }

  /*
   * Gardé par `:not(.shut)`, et c'est la clause qui compte.
   *
   * Le nom d'un appareil passe à la ligne, donc son entrée s'aligne en haut.
   * Sans cette garde, la règle **s'appliquerait** en état replié et il faudrait
   * qu'elle perde un arbitrage de spécificité contre le masquage — exactement le
   * piège dans lequel cette interface est déjà tombée.
   */
  .devices:not(.shut) .entry {
    align-items: flex-start;
  }

  .devices:not(.shut) .glyph {
    margin-top: 3px;
  }
}

/*
 * ------------------------------------------------------------------ réglages
 */
.detail {
  overflow-y: auto;
  padding: var(--gap-4);
  gap: var(--gap-3);
  background: var(--ground);
  border-right: none;
}

.fx-head {
  display: flex;
  flex-wrap: wrap;
  gap: var(--gap-2);
  align-items: baseline;
}

.fx-head h1 {
  min-width: 0;
  overflow-wrap: anywhere;
}

.badge {
  flex: none;
  padding: 2px var(--gap-2);
  border-radius: 99px;
  background: var(--raised-2);
  color: var(--text-muted);
  font-size: 11px;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}

.badge.user {
  background: var(--accent-soft);
  color: var(--accent);
}

.badge.hardware {
  background: color-mix(in srgb, var(--ok) 14%, transparent);
  color: var(--ok);
}

/* Not a warning: a fact about what the effect reads, said where it is chosen. */
.badge.keys {
  border: 1px solid var(--line-strong);
  background: none;
}

.desc {
  max-width: 68ch;
  color: var(--text-muted);
  font-size: 13px;
}

/* Le coût réel, les notes d'état et les aides : même voix, la plus discrète. */
.cost {
  max-width: 68ch;
  color: var(--text-faint);
  font-size: 12px;
}

.preview {
  display: flex;
  flex-direction: column;
  gap: var(--gap-2);
}

/* Le dessin ne prend pas plus que sa part : la colonne porte aussi les réglages
   et les actions, et un clavier qui pousse le reste hors de l'écran ferait
   défiler pour trouver un bouton. Borné en **largeur** et non en hauteur — le
   SVG garde ses proportions, une hauteur maximale le ferait rogner. */
.sim {
  max-width: 760px;
}

.actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--gap-2);
  align-items: center;
  margin-top: auto;
  padding-top: var(--gap-3);
  border-top: 1px solid var(--line);
}

.spacer {
  flex: 1;
}

.solid {
  padding: 6px var(--gap-3);
  background: var(--accent);
  border-radius: var(--r-md);
  color: var(--accent-ink);
  font-size: 13px;
  font-weight: 500;
}

.ghost {
  padding: 6px var(--gap-3);
  border: 1px solid var(--line-strong);
  border-radius: var(--r-md);
  color: var(--text-muted);
  font-size: 13px;
}

.ghost:hover:not(:disabled) {
  color: var(--text);
  background: var(--raised-2);
}

.solid:disabled,
.ghost:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

/*
 * Un geste sans retour. La couleur ne le dit pas seule — le libellé annonce la
 * suppression, et la confirmation énumère ce qui part : un daltonien lit la même
 * chose que les autres.
 */
.danger {
  color: var(--bad);
  border-color: var(--bad);
}

.solid.danger {
  background: var(--bad);
  color: var(--accent-ink);
}

.ghost.danger:hover:not(:disabled) {
  color: var(--bad);
  background: color-mix(in srgb, var(--bad) 12%, transparent);
}

.confirm {
  display: flex;
  flex-direction: column;
  gap: var(--gap-2);
  padding: var(--gap-3);
  border: 1px solid var(--bad);
  border-radius: var(--r-md);
  background: color-mix(in srgb, var(--bad) 8%, var(--raised));
}

.confirm-title {
  font-weight: 600;

  /* Un nom d'effet est libre : il passe à la ligne plutôt que de déborder. */
  overflow-wrap: anywhere;
}

.confirm-actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--gap-2);
}

.notice,
.failure {
  padding: var(--gap-3);
  border-radius: var(--r-md);
  font-size: 13px;
}

.notice {
  background: var(--raised);
  border: 1px solid var(--line);
  color: var(--text-muted);
}

/* Un écart entre ce qu'on a demandé et ce qui se passe. La couleur ne porte pas
   seule : le texte le dit aussi. */
.notice.warn {
  background: color-mix(in srgb, var(--warn) 12%, var(--raised));
  border-color: var(--warn);
  color: var(--text);
}

.failure {
  background: color-mix(in srgb, var(--bad) 10%, transparent);
  border: 1px solid var(--bad);
}

.none {
  color: var(--text-faint);
  font-size: 12px;
}

.link {
  color: var(--accent);
  text-decoration: none;
  white-space: nowrap;
}

.link:hover {
  text-decoration: underline;
}

/*
 * Fenêtre étroite : les trois colonnes s'empilent. Le repliement n'y a plus de
 * sens — une colonne pleine largeur réduite à une bande d'icônes ne gagnerait
 * rien — donc ses règles sont **entièrement** dans le point de rupture large, et
 * le bouton disparaît plutôt que de basculer un état sans effet.
 */
@media (width <= 820px) {
  .studio {
    grid-template-columns: minmax(0, 1fr);
    grid-template-rows: auto auto minmax(0, 1fr);
    overflow-y: auto;
  }

  .col {
    border-right: none;
    border-bottom: 1px solid var(--line);
  }

  .detail {
    border-bottom: none;
  }

  .collapse {
    display: none;
  }

  /* Les deux listes cèdent la place au contenu, sans disparaître. */
  .devices .col-body,
  .effects .col-body {
    max-height: 24vh;
  }
}
</style>
