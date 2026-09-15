// Ripples — a ring spreads from every key pressed and fades as it widens.
//
// It reads key presses (`inputs: ['keys']`), which Candeo captures only while an
// effect like this one runs. Each press comes with the instant it happened, so a
// ring's radius is its age times the speed: nothing is kept between frames, and
// the preview draws the same rings as the keyboard.
//
// Distances are physical, so a ring stays round across the keyboard. At rest the
// keyboard shows the background color, which is also what the swatch samples.

import { center, defineEffect, mix } from '@candeo/effects-api'

const RING = { r: 64, g: 200, b: 255 }
const BACKGROUND = { r: 6, g: 12, b: 28 }

export default defineEffect({
  description: {
    en: 'A ring spreads from every key you press and fades as it widens',
    fr: "Un anneau part de chaque touche pressée et s'efface en grandissant",
  },
  kinds: ['keyboard'],
  inputs: ['keys'],
  params: {
    color: { kind: 'color', label: { en: 'Ring', fr: 'Anneau' }, default: RING },
    background: { kind: 'color', label: { en: 'Background', fr: 'Fond' }, default: BACKGROUND },
    speed: {
      kind: 'number',
      label: { en: 'Speed (keys per second)', fr: 'Vitesse (touches par seconde)' },
      min: 2,
      max: 40,
      step: 1,
      default: 12,
    },
    width: {
      kind: 'number',
      label: { en: 'Ring width (keys)', fr: 'Épaisseur (touches)' },
      min: 0.5,
      max: 4,
      step: 0.5,
      default: 1.5,
    },
    lifetime: {
      kind: 'number',
      label: { en: 'Lifetime (s)', fr: 'Durée (s)' },
      min: 0.3,
      max: 5,
      step: 0.1,
      default: 1.2,
    },
  },
  render({ layout, time, presses, frame, params }) {
    const color = params.color ?? RING
    const background = params.background ?? BACKGROUND
    const speed = Number(params.speed ?? 12)
    const width = Number(params.width ?? 1.5)
    const lifetime = Number(params.lifetime ?? 1.2)

    const rings = presses
      .map(({ key, at }) => ({ origin: center(key), age: time - at }))
      .filter((ring) => ring.age >= 0 && ring.age < lifetime)

    for (const key of layout.keys) {
      const c = center(key)
      let strength = 0
      for (const ring of rings) {
        const distance = Math.hypot(c.x - ring.origin.x, c.y - ring.origin.y)
        // Lit on the circle, dark inside and outside; fading with age.
        const onRing = Math.max(0, 1 - Math.abs(distance - ring.age * speed) / width)
        strength = Math.max(strength, onRing * (1 - ring.age / lifetime))
      }
      frame.set(key, mix(background, color, strength))
    }
  },
})
