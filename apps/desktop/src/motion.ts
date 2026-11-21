import type { Transition } from 'motion/react'

const popPreset = { damping: 1, response: .38 }
const smoothPreset = { damping: 1, response: .32 }

function toTransition({ damping, response }: { damping: number; response: number }): Transition {
  const stiffness = (2 * Math.PI / response) ** 2
  return { type: 'spring', stiffness, damping: 4 * Math.PI * damping / response, mass: 1 }
}

export const smooth = toTransition(smoothPreset)
export const pop = toTransition(popPreset)

function springCurve({ damping, response }: { damping: number; response: number }) {
  const stiffness = (2 * Math.PI / response) ** 2
  const friction = 4 * Math.PI * damping / response
  const step = 1 / 240
  const samples = [0]
  let position = 0, velocity = 0, time = 0
  while (time < 2.5) {
    for (let i = 0; i < 4; i++) {
      velocity += (-stiffness * (position - 1) - friction * velocity) * step
      position += velocity * step
    }
    time += step * 4
    samples.push(position)
    if (time > .15 && Math.abs(position - 1) < .002 && Math.abs(velocity) < .02) break
  }
  samples[samples.length - 1] = 1
  const count = Math.min(samples.length, 64)
  const points = Array.from({ length: count }, (_, i) => samples[Math.round(i * (samples.length - 1) / (count - 1))].toFixed(4))
  return { easing: `linear(${points.join(', ')})`, duration: Math.round(time * 1000) }
}

export function applyCssSprings() {
  const pop = springCurve(popPreset)
  const calm = springCurve(smoothPreset)
  const root = document.documentElement.style
  root.setProperty('--spring', pop.easing)
  root.setProperty('--spring-dur', `${pop.duration}ms`)
  root.setProperty('--smooth', calm.easing)
  root.setProperty('--smooth-dur', `${calm.duration}ms`)
  root.setProperty('--disclosure-duration', `${Math.round(calm.duration * .6)}ms`)
}
