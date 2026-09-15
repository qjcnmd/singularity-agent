import { useEffect, useRef, useState } from 'react'
import { createGalaxyRenderer } from './galaxyRenderer'
import { createPearlRenderer } from './pearlRenderer'

const normalSpeed = 6
const activeSpeed = normalSpeed * 3

function OrbCanvas({ fast, theme }: { fast: boolean; theme: 'light' | 'dark' }) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const speed = useRef(fast ? activeSpeed : normalSpeed)
  useEffect(() => { speed.current = fast ? activeSpeed : normalSpeed }, [fast])
  useEffect(() => {
    if (!canvas.current) return
    const render = theme === 'dark' ? createGalaxyRenderer(canvas.current) : createPearlRenderer(canvas.current)
    render(0)
    const motion = window.matchMedia('(prefers-reduced-motion: reduce)')
    let frame = 0
    let last: number | null = null
    const tick = (now: number) => {
      const elapsed = last === null ? 0 : Math.min((now - last) / 1000, 0.1)
      last = now
      // 将速度积分进已用时间；改变它绝不改变当前 phase。
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
  }, [theme])
  return <canvas ref={canvas} className="orb-canvas" aria-hidden="true" />
}

export function ActivityOrb({ fast = false, theme = 'light' }: { fast?: boolean; theme?: 'light' | 'dark' }) {
  const [hovered, setHovered] = useState(false)
  const accelerated = fast || hovered
  return <span className="activity-orb" aria-hidden="true" onPointerEnter={() => setHovered(true)} onPointerLeave={() => setHovered(false)}>
    <OrbCanvas fast={accelerated} theme={theme} />
  </span>
}
