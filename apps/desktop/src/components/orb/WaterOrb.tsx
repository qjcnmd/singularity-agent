import { useEffect, useRef } from 'react'

type WaterOrbProps = {
  /** 上下文占用，按高度映射，0–100。 */
  percent: number
  rotating: boolean
  enabled: boolean
  size: number
  label: string
  color: string
  onPress?: () => void
}

const TAU = Math.PI * 2
const CALM_INTERVAL_MS = 1000 / 24
const clamp01 = (value: number) => Math.max(0, Math.min(1, Number.isFinite(value) ? value : 0))

function parseRgb(color: string): [number, number, number] {
  const trimmed = color.trim()
  if (trimmed.startsWith('#')) {
    const hex = trimmed.slice(1)
    if (hex.length === 3 || hex.length === 4) {
      return [parseInt(hex[0] + hex[0], 16), parseInt(hex[1] + hex[1], 16), parseInt(hex[2] + hex[2], 16)]
    }
    if (hex.length >= 6) {
      const value = parseInt(hex.slice(0, 6), 16)
      if (!Number.isNaN(value)) return [(value >> 16) & 255, (value >> 8) & 255, value & 255]
    }
  }
  const match = trimmed.match(/rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)/i)
  if (match) {
    return [Number(match[1]) || 0, Number(match[2]) || 0, Number(match[3]) || 0]
  }
  return [23, 131, 255]
}

function buildGlassShell(pixelSize: number): HTMLCanvasElement {
  const shell = document.createElement('canvas')
  shell.width = pixelSize
  shell.height = pixelSize
  const ctx = shell.getContext('2d')!
  const half = pixelSize / 2
  ctx.setTransform(half, 0, 0, half, half, half)

  ctx.save()
  ctx.beginPath()
  ctx.arc(0, 0, 0.99, 0, TAU)
  ctx.clip()

  const volume = ctx.createRadialGradient(-0.28, -0.38, 0.08, 0.08, 0.12, 1.28)
  volume.addColorStop(0, 'rgba(255,255,255,.25)')
  volume.addColorStop(0.42, 'rgba(255,255,255,.06)')
  volume.addColorStop(0.66, 'rgba(178,218,255,.02)')
  volume.addColorStop(0.86, 'rgba(167,213,255,.05)')
  volume.addColorStop(1, 'rgba(155,207,255,.08)')
  ctx.fillStyle = volume
  ctx.fillRect(-1, -1, 2, 2)

  const rim = ctx.createRadialGradient(0, 0, 0.73, 0, 0, 1.01)
  rim.addColorStop(0, 'rgba(255,255,255,0)')
  rim.addColorStop(0.53, 'rgba(194,227,255,.03)')
  rim.addColorStop(0.76, 'rgba(183,223,255,.06)')
  rim.addColorStop(0.89, 'rgba(245,252,255,.78)')
  rim.addColorStop(1, 'rgba(172,218,255,.20)')
  ctx.fillStyle = rim
  ctx.fillRect(-1, -1, 2, 2)

  const transmitted = ctx.createRadialGradient(0.18, 0.84, 0, 0.18, 0.84, 0.48)
  transmitted.addColorStop(0, 'rgba(236,250,255,.30)')
  transmitted.addColorStop(1, 'rgba(236,250,255,0)')
  ctx.fillStyle = transmitted
  ctx.fillRect(-1, -1, 2, 2)

  ctx.beginPath()
  ctx.arc(-0.23, -0.84, 0.035, 0, TAU)
  ctx.fillStyle = '#fff'
  ctx.fill()
  ctx.restore()

  ctx.beginPath()
  ctx.arc(0, 0, 0.99, 0, TAU)
  ctx.strokeStyle = 'rgba(159,210,255,.30)'
  ctx.lineWidth = 0.013
  ctx.stroke()

  return shell
}

interface OrbBubble {
  x: number
  y: number
  radius: number
  speed: number
  phase: number
}

function createBubbles(): OrbBubble[] {
  return Array.from({ length: 8 }, (_, index) => ({
    x: -0.58 + (index * 0.165),
    y: 0.9 - (index % 4) * 0.32,
    radius: 0.018 + (index % 3) * 0.011,
    speed: 0.28 + (index % 4) * 0.08,
    phase: index * 1.1,
  }))
}

export function WaterOrb({ percent, rotating, enabled, size, label, color, onPress }: WaterOrbProps) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const wakeRef = useRef<(() => void) | null>(null)
  const latest = useRef({ occupancy: clamp01(percent / 100), rotating, rgb: parseRgb(color) })
  latest.current = { occupancy: clamp01(percent / 100), rotating, rgb: parseRgb(color) }

  useEffect(() => {
    const element = canvas.current!
    const ctx = element.getContext('2d')!
    const reduced = matchMedia('(prefers-reduced-motion: reduce)')
    const dpr = Math.min(devicePixelRatio || 1, 2)
    const pixelSize = Math.max(1, Math.round(size * dpr))
    element.width = pixelSize
    element.height = pixelSize

    const shell = buildGlassShell(pixelSize)
    const bubbles = createBubbles()
    let level = latest.current.occupancy
    let turbulence = latest.current.rotating ? 1 : 0
    let phase = 0
    let tilt = 0
    let lastTick = 0
    let lastPaint = 0
    let frame = 0

    const drawFrame = (now: number) => {
      frame = 0
      const { occupancy, rotating: spinning, rgb } = latest.current
      const calmIdle = !spinning && Math.abs(level - occupancy) < 0.002 && turbulence < 0.02
      const emptySettled = calmIdle && occupancy <= 0.002 && level <= 0.002

      const dt = lastTick ? Math.min((now - lastTick) / 1000, 0.05) : 0.016
      lastTick = now

      if (reduced.matches) {
        level = occupancy
        turbulence = 0
      } else {
        level += (occupancy - level) * Math.min(1, dt * 4.5)
        turbulence += ((spinning ? 1 : 0) - turbulence) * Math.min(1, dt * 3.5)
        phase += dt * (2.2 + turbulence * 5.2)
        tilt = turbulence > 0.01 ? Math.sin(phase * 0.55) * 0.24 * turbulence : tilt * Math.max(0, 1 - dt * 5)
        for (const bubble of bubbles) {
          bubble.y -= dt * bubble.speed * (0.45 + turbulence * 1.4)
          if (bubble.y < -0.75) bubble.y = 0.92
        }
      }

      if (!calmIdle || reduced.matches || emptySettled || now - lastPaint >= CALM_INTERVAL_MS || lastPaint === 0) {
        lastPaint = now
        ctx.setTransform(1, 0, 0, 1, 0, 0)
        ctx.clearRect(0, 0, pixelSize, pixelSize)

        const visualLevel = Math.max(level, turbulence * 0.18)
        if (visualLevel > 0.002) {
          const half = pixelSize / 2
          const [r, g, b] = rgb
          const surfaceY = 0.96 - visualLevel * 1.88
          const amp1 = reduced.matches ? 0 : 0.016 + turbulence * 0.055
          const amp2 = reduced.matches ? 0 : 0.009 + turbulence * 0.032

          ctx.save()
          ctx.setTransform(half, 0, 0, half, half, half)
          ctx.beginPath()
          ctx.arc(0, 0, 0.96, 0, TAU)
          ctx.clip()

          const body = ctx.createLinearGradient(0, surfaceY - 0.1, 0, 1)
          body.addColorStop(0, `rgba(${r}, ${g}, ${b}, 0.72)`)
          body.addColorStop(0.55, `rgba(${r}, ${g}, ${b}, 0.9)`)
          body.addColorStop(1, `rgba(${Math.round(r * 0.72)}, ${Math.round(g * 0.72)}, ${Math.round(b * 0.72)}, 0.98)`)

          ctx.beginPath()
          const steps = 36
          for (let i = 0; i <= steps; i++) {
            const x = -1.02 + (2.04 * i) / steps
            const y = surfaceY
              + tilt * x
              + Math.sin(x * 3.1 + phase) * amp1
              + Math.cos(x * 5.3 - phase * 1.35) * amp2
            if (i === 0) ctx.moveTo(x, y)
            else ctx.lineTo(x, y)
          }
          ctx.lineTo(1.04, 1.04)
          ctx.lineTo(-1.04, 1.04)
          ctx.closePath()
          ctx.fillStyle = body
          ctx.fill()

          // 旋转时加几颗轻微上浮的气泡和液面高光线
          if (!reduced.matches && visualLevel > 0.06) {
            ctx.beginPath()
            for (let i = 0; i <= steps; i++) {
              const x = -1.02 + (2.04 * i) / steps
              const y = surfaceY
                + tilt * x
                + Math.sin(x * 3.1 + phase) * amp1
                + Math.cos(x * 5.3 - phase * 1.35) * amp2
              if (i === 0) ctx.moveTo(x, y)
              else ctx.lineTo(x, y)
            }
            ctx.strokeStyle = 'rgba(255, 255, 255, 0.34)'
            ctx.lineWidth = 0.018
            ctx.stroke()

            if (turbulence > 0.05) {
              ctx.fillStyle = `rgba(255, 255, 255, ${(0.28 * turbulence).toFixed(3)})`
              for (const bubble of bubbles) {
                const bx = bubble.x + Math.sin(phase * 0.8 + bubble.phase) * 0.05
                if (bubble.y > surfaceY + 0.06) {
                  ctx.beginPath()
                  ctx.arc(bx, bubble.y, bubble.radius, 0, TAU)
                  ctx.fill()
                }
              }
            }
          }
          ctx.restore()
        }

        ctx.setTransform(1, 0, 0, 1, 0, 0)
        ctx.drawImage(shell, 0, 0)
      }

      if (!reduced.matches && !document.hidden && !emptySettled) {
        frame = requestAnimationFrame(drawFrame)
      }
    }

    const wake = () => {
      cancelAnimationFrame(frame)
      frame = 0
      lastTick = 0
      lastPaint = 0
      if (!document.hidden) {
        frame = requestAnimationFrame(drawFrame)
      }
    }

    wakeRef.current = wake
    wake()
    document.addEventListener('visibilitychange', wake)
    reduced.addEventListener('change', wake)
    return () => {
      cancelAnimationFrame(frame)
      document.removeEventListener('visibilitychange', wake)
      reduced.removeEventListener('change', wake)
      wakeRef.current = null
    }
  }, [size])

  useEffect(() => {
    wakeRef.current?.()
  }, [percent, rotating, color])

  return (
    <button
      type="button"
      className="water-orb"
      style={{ width: size, height: size }}
      aria-label={label}
      aria-disabled={!enabled}
      onClick={() => { if (enabled) onPress?.() }}
    >
      <canvas ref={canvas} style={{ width: size, height: size }} aria-hidden="true" />
    </button>
  )
}
