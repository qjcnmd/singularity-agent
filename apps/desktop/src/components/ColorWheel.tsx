import { useRef, type KeyboardEvent } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { palette, type PaletteColor } from '../bubbleColors'
import { pop } from '../motion'

const size = 84
/** 球心所在圆的半径。 */
const ring = 30
const ball = 18

/** 某一格在这个调色盘里的样子和名字，不同于调色盘默认的实色时传入（比如助手消息的“灰黑”格实际是白底）。 */
export interface BallLook { label: string; ball: string; rim: string }

/** 一圈玻璃球调色盘：打开时从中心依次弹到各自位置，9 种颜色从正上方顺时针排开。
 * chosen 有值时其他球淡出（给调用方做“滴入”动画留出画面）。方向键在球之间移动。 */
export function ColorWheel({ value, label, chosen = null, looks = {}, onSelect }: {
  value: string
  label: string
  chosen?: string | null
  looks?: Partial<Record<string, BallLook>>
  onSelect: (color: PaletteColor, ball: HTMLElement) => void
}) {
  const root = useRef<HTMLDivElement>(null)
  const reduced = useReducedMotion()
  const step = Math.PI * 2 / palette.length
  const hasValue = palette.some(color => color.id === value)
  const move = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return
    event.preventDefault()
    const items = [...root.current!.querySelectorAll<HTMLButtonElement>('[role=radio]')]
    const index = items.indexOf(document.activeElement as HTMLButtonElement)
    const forward = event.key === 'ArrowRight' || event.key === 'ArrowDown'
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 : (index + (forward ? 1 : -1) + items.length) % items.length
    items[next]?.focus()
  }
  return <div ref={root} className="color-wheel" style={{ width: size, height: size }} role="radiogroup" aria-label={label} onKeyDown={move}>
    {palette.map((color, index) => {
      const angle = index * step - Math.PI / 2
      const selected = color.id === value
      const hidden = chosen !== null
      const look = looks[color.id]
      return <motion.button key={color.id} type="button" className="color-ball" role="radio" aria-checked={selected} aria-label={look?.label ?? color.label} title={look?.label ?? color.label}
        tabIndex={selected || (!hasValue && index === 0) ? 0 : -1} disabled={hidden}
        style={{ width: ball, height: ball, left: size / 2 - ball / 2, top: size / 2 - ball / 2, '--ball': look?.ball ?? color.solid, '--ball-rim': look?.rim ?? color.solid } as React.CSSProperties}
        initial={reduced ? false : { x: 0, y: 0, scale: .2, opacity: 0 }}
        animate={{ x: Math.cos(angle) * ring, y: Math.sin(angle) * ring, scale: 1, opacity: hidden ? 0 : 1 }}
        transition={reduced ? { duration: 0 } : { ...pop, delay: hidden ? 0 : index * .018, opacity: { duration: hidden ? .16 : .12, delay: hidden ? 0 : index * .018 } }}
        onClick={event => onSelect(color, event.currentTarget)} />
    })}
  </div>
}
