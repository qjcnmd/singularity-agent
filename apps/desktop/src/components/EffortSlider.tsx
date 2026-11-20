import { useEffect, useRef, useState } from 'react'
import { animate, motion, useMotionValue, useMotionValueEvent, useReducedMotion, useTransform } from 'motion/react'

const thumbSize = 24
const thumbInset = 2
const edge = thumbSize / 2 + thumbInset
const sliderSpring = { type: 'spring', stiffness: 420, damping: 30, mass: 0.8 } as const
const clamp = (value: number) => Math.min(1, Math.max(0, value))
const along = (ratio: number) => `calc((100% - ${edge * 2}px) * ${ratio} + ${edge}px)`
const filled = (ratio: number) => `calc((100% - ${edge * 2}px) * ${ratio} + ${edge * 2}px)`

export function EffortSlider({ variants, selectedIndex, onCommit }: {
  variants: string[]
  selectedIndex: number
  onCommit: (index: number) => void
}) {
  const last = Math.max(1, variants.length - 1)
  const reduced = useReducedMotion()
  const transition = reduced ? { duration: 0 } : sliderSpring
  const track = useRef<HTMLDivElement>(null)
  const drag = useRef<{ grabOffset: number } | null>(null)
  const ratio = useMotionValue(selectedIndex / last)
  const [liveIndex, setLiveIndex] = useState(selectedIndex)
  const [pressed, setPressed] = useState(false)

  useMotionValueEvent(ratio, 'change', value => setLiveIndex(Math.round(clamp(value) * last)))
  useEffect(() => {
    if (!drag.current) animate(ratio, selectedIndex / last, transition)
  }, [selectedIndex, last, ratio, reduced])

  const fillWidth = useTransform(ratio, filled)
  const thumbLeft = useTransform(ratio, along)

  const ratioAt = (clientX: number, grabOffset: number) => {
    const rect = track.current!.getBoundingClientRect()
    return clamp((clientX - grabOffset - rect.left - edge) / Math.max(1, rect.width - edge * 2))
  }

  const press = (event: React.PointerEvent) => {
    if (event.button !== 0 || event.currentTarget.closest('fieldset:disabled')) return
    event.currentTarget.setPointerCapture(event.pointerId)
    const rect = track.current!.getBoundingClientRect()
    const thumbCenter = rect.left + edge + (rect.width - edge * 2) * ratio.get()
    const onThumb = Math.abs(event.clientX - thumbCenter) <= thumbSize / 2 + 2
    drag.current = { grabOffset: onThumb ? event.clientX - thumbCenter : 0 }
    setPressed(true)
    const next = ratioAt(event.clientX, drag.current.grabOffset)
    setLiveIndex(Math.round(next * last))
    if (onThumb) {
      ratio.stop()
      ratio.set(next)
    } else {
      animate(ratio, next, transition)
    }
  }

  const move = (event: React.PointerEvent) => {
    if (!drag.current) return
    const next = ratioAt(event.clientX, drag.current.grabOffset)
    ratio.stop()
    ratio.set(next)
    setLiveIndex(Math.round(next * last))
  }

  const release = (event: React.PointerEvent) => {
    if (!drag.current) return
    const current = event.type === 'pointercancel' ? ratio.get() : ratioAt(event.clientX, drag.current.grabOffset)
    const index = Math.min(variants.length - 1, Math.round(current * last))
    drag.current = null
    setPressed(false)
    setLiveIndex(index)
    animate(ratio, index / last, transition)
    onCommit(index)
  }

  return (
    <div className="effort">
      <div className="effort-head">{variants[liveIndex]}</div>
      <div ref={track} className="track" onPointerDown={press} onPointerMove={move} onPointerUp={release} onPointerCancel={release}>
        <motion.div className="track-fill" style={{ width: fillWidth }} />
        <div className="track-stops" aria-hidden="true">
          {variants.map((variant, index) => (
            <span
              key={variant}
              className="track-stop"
              data-covered={index < liveIndex}
              style={{ left: along(index / last) }}
            />
          ))}
        </div>
        <motion.div className="track-thumb" style={{ left: thumbLeft }} data-pressed={pressed} />
        <input
          type="range"
          min={0}
          max={variants.length - 1}
          step={1}
          value={selectedIndex}
          aria-label="思考程度"
          aria-valuetext={variants[selectedIndex]}
          onChange={event => onCommit(Number(event.target.value))}
        />
      </div>
    </div>
  )
}
