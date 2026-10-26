import type { Transition } from 'motion/react'

const popPreset = { damping: 1, response: .38 }
const smoothPreset = { damping: 1, response: .32 }

function toTransition({ damping, response }: { damping: number; response: number }): Transition {
  const stiffness = (2 * Math.PI / response) ** 2
  return { type: 'spring', stiffness, damping: 4 * Math.PI * damping / response, mass: 1 }
}

export const smooth = toTransition(smoothPreset)
export const pop = toTransition(popPreset)
export const actionToastTransition = pop
/** 消息、卡片和弹出的调色盘共用的出现动画：从下方略缩小处弹出。 */
export const enter = { initial: { opacity: 0, y: 10, scale: .9 }, animate: { opacity: 1, y: 0, scale: 1 } }
export const disclosureTransition = (_expanded: boolean, reduced: boolean | null): Transition => reduced ? { duration: 0 } : smooth

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
  for (const name of ['--disclosure-duration', '--disclosure-collapse-duration']) root.setProperty(name, `${Math.round(calm.duration * .6)}ms`)
  for (const name of ['--disclosure-ease', '--disclosure-collapse-ease']) root.setProperty(name, calm.easing)
}
