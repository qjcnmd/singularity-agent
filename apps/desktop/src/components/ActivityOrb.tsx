import { useEffect, useRef, useState } from 'react'
import { createPearlRenderer } from './pearlRenderer'

const normalSpeed = 6
const activeSpeed = normalSpeed * 3

function OrbCanvas({ fast }: { fast: boolean }) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const speed = useRef(fast ? activeSpeed : normalSpeed)
  useEffect(() => { speed.current = fast ? activeSpeed : normalSpeed }, [fast])
  useEffect(() => {
    if (!canvas.current) return
    const render = createPearlRenderer(canvas.current)
    render(0)
    const motion = window.matchMedia('(prefers-reduced-motion: reduce)')
    let frame = 0
    let last: number | null = null
    const tick = (now: number) => {
      const elapsed = last === null ? 0 : Math.min((now - last) / 1000, 0.1)
      last = now
      // 速度按已用时间积分，改速度不影响当前 phase。
      render(elapsed * speed.current)
      frame = requestAnimationFrame(tick)
    }
    const resume = () => {
      cancelAnimationFrame(frame)
      last = null
      if (!motion.matches && !document.hidden) frame = requestAnimationFrame(tick)
    }
    resume()
    motion.addEventListener('change', resume)
    document.addEventListener('visibilitychange', resume)
    return () => {
      cancelAnimationFrame(frame)
      motion.removeEventListener('change', resume)
      document.removeEventListener('visibilitychange', resume)
    }
  }, [])
  return <canvas ref={canvas} className="orb-canvas" aria-hidden="true" />
}

export function ActivityOrb({ fast = false }: { fast?: boolean }) {
  const [hovered, setHovered] = useState(false)
  const accelerated = fast || hovered
  return <span className="activity-orb" aria-hidden="true" onPointerEnter={() => setHovered(true)} onPointerLeave={() => setHovered(false)}>
    <OrbCanvas fast={accelerated} />
  </span>
}
