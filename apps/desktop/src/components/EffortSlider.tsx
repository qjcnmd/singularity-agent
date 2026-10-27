import { useEffect, useRef, useState, type RefObject } from 'react'
import { animate, motion, useReducedMotion, useAnimationFrame, useMotionValue, useMotionValueEvent, useTransform, type MotionValue } from 'motion/react'

/** 滑块直径；滑块嵌在轨道里，四周留 thumbInset 的边。 */
const thumbSize = 18
const thumbInset = 3
/** 滑块中心到轨道两端的最小距离。 */
const edge = thumbSize / 2 + thumbInset
/** 松手吸附到档位、点击轨道跳转用的弹簧：带一点回弹。拖动时不用弹簧，滑块和鼠标 1:1。 */
const sliderSpring = { type: 'spring', stiffness: 420, damping: 30, mass: .8 } as const
const clamp = (value: number) => Math.min(1, Math.max(0, value))
/** 滑块中心沿轨道的位置：从 edge 到 100% - edge。 */
const along = (ratio: number) => `calc((100% - ${edge * 2}px) * ${ratio} + ${edge}px)`
/** 已选部分一直铺到滑块右边缘外的那圈边。 */
const filled = (ratio: number) => `calc((100% - ${edge * 2}px) * ${ratio} + ${edge * 2}px)`

/** 档位按在全部档位里的位置取色：最低青、偏低蓝、偏高粉、最高紫；只有一两个档位时也按位置取。 */
const levels = [
  { ink: '#0f8fa3', fill: 'linear-gradient(90deg, #5fd3e3, #22b8cf)' },
  { ink: '#1f5fd0', fill: 'linear-gradient(90deg, #7fb0ff, #3b82f6)' },
  { ink: '#d14b8f', fill: 'linear-gradient(90deg, #f7a8cc, #ec72ad)' },
  { ink: '#7c3aed', fill: 'linear-gradient(90deg, #9333ea, #c084fc 70%, #a855f7)' },
]
const levelAt = (index: number, last: number) => levels[Math.round(index / last * (levels.length - 1))]

/** 思考程度滑块。拖动时直接跟着指针（按住滑块的位置保持不变），松手后用弹簧吸附到最近的档位；
 * 填充长度、闪点的数量和速度都读同一个 ratio。键盘用隐藏的 range 输入，左右键切档。 */
export function EffortSlider({ variants, selectedIndex, onCommit }: { variants: string[]; selectedIndex: number; onCommit: (index: number) => void }) {
  const last = Math.max(1, variants.length - 1)
  const reduced = useReducedMotion()
  const transition = reduced ? { duration: 0 } : sliderSpring
  const track = useRef<HTMLDivElement>(null)
  const drag = useRef<{ grabOffset: number } | null>(null)
  const ratio = useMotionValue(selectedIndex / last)
  const [liveIndex, setLiveIndex] = useState(selectedIndex)
  const [pressed, setPressed] = useState(false)
  useMotionValueEvent(ratio, 'change', value => setLiveIndex(Math.round(clamp(value) * last)))
  useEffect(() => { if (!drag.current) animate(ratio, selectedIndex / last, transition) }, [selectedIndex, last, ratio, reduced])

  const fillWidth = useTransform(ratio, filled)
  const thumbLeft = useTransform(ratio, along)

  const ratioAt = (clientX: number, grabOffset: number) => {
    const rect = track.current!.getBoundingClientRect()
    return clamp((clientX - grabOffset - rect.left - edge) / (rect.width - edge * 2))
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
    if (onThumb) { ratio.stop(); ratio.set(next) } else animate(ratio, next, transition)
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
    const index = Math.min(variants.length - 1, Math.round((event.type === 'pointercancel' ? ratio.get() : ratioAt(event.clientX, drag.current.grabOffset)) * last))
    drag.current = null
    setPressed(false)
    setLiveIndex(index)
    animate(ratio, index / last, transition)
    onCommit(index)
  }

  const level = levelAt(liveIndex, last)
  return <div className="effort">
    <div className="effort-head" style={{ color: level.ink }}>{variants[liveIndex]}</div>
    <div ref={track} className="track" onPointerDown={press} onPointerMove={move} onPointerUp={release} onPointerCancel={release}>
      <motion.div className="track-fill" style={{ width: fillWidth, background: level.fill }}>
        <Sparks ratio={ratio} track={track} />
      </motion.div>
      <motion.div className="track-thumb" style={{ left: thumbLeft }} data-pressed={pressed} />
      <input type="range" min={0} max={variants.length - 1} step={1} value={selectedIndex} aria-label="思考程度" aria-valuetext={variants[selectedIndex]}
        onChange={event => onCommit(Number(event.target.value))} />
    </div>
  </div>
}

type Spark = { x: number; y: number; vx: number; size: number; age: number; life: number }
const sparkPool = 18
/** 像往右冲刺时甩在身后的尾气：闪点从滑块后面冒出，向左飘走并逐渐淡出。
 * 思考程度越高，冒得越密、飘得越快。 */
function Sparks({ ratio, track }: { ratio: MotionValue<number>; track: RefObject<HTMLDivElement | null> }) {
  const nodes = useRef<(HTMLElement | null)[]>([])
  const items = useRef<Spark[]>([])
  const credit = useRef(0)
  const still = useReducedMotion()
  useAnimationFrame((_, elapsed) => {
    if (still) return
    const dt = Math.min(elapsed, 50) / 1000
    const level = clamp(ratio.get())
    const width = track.current?.offsetWidth ?? 0
    const fill = (width - edge * 2) * level + edge * 2
    const height = track.current?.offsetHeight ?? 0
    credit.current += dt * (5 + level * 25)
    while (credit.current >= 1) {
      credit.current -= 1
      if (fill > 40 && items.current.length < Math.round(5 + level * 13)) {
        const speed = (50 + level * 190) * (.7 + Math.random() * .6)
        items.current.push({ x: fill - edge - Math.random() * 8, y: 4 + Math.random() * (height - 8), vx: -speed, size: 1.4 + Math.random() * 1.8, age: 0, life: (fill - 10) / speed })
      }
    }
    for (const spark of items.current) { spark.age += dt; spark.x += spark.vx * dt }
    items.current = items.current.filter(spark => spark.x > 2 && spark.age < spark.life)
    nodes.current.forEach((node, index) => {
      const spark = items.current[index]
      if (!node) return
      if (!spark) { node.style.opacity = '0'; return }
      // 冒出时 0.08 秒淡入，之后越往左越淡。
      const alpha = Math.min(1, spark.age / .08) * (1 - spark.age / spark.life) * .9
      node.style.opacity = String(Math.max(0, alpha))
      node.style.transform = `translate(${spark.x}px, ${spark.y}px) scale(${spark.size / 3})`
    })
  })
  return <>{Array.from({ length: sparkPool }, (_, index) => <i key={index} ref={node => { nodes.current[index] = node }} className="spark" />)}</>
}
