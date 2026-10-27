import { useEffect, useRef } from 'react'
import { useSelectionGuard } from '../../interactions'
import { Fluid } from './fluid'
import { Bubbles } from './bubbles'

/* 水球发送键（来自用户提供的 singularity-water-orb 原型）：
 * 平时是解析出来的起伏海面；压缩时球壁转动，水改由 LiquidFun 粒子重建并翻滚；结束后平息再回落到新水位。
 * 球上不画箭头，点击即发送，运行中点击即停止。 */

export type WaterOrbProps = {
  /** 上下文占用，按高度映射，0–100。 */
  percent: number
  compacting: boolean
  enabled: boolean
  size: number
  label: string
  color: string
  onPress?: () => void
}

const TAU = Math.PI * 2
const resolution = 200
const clamp = (value: number) => Math.max(0, Math.min(1, Number.isFinite(value) ? value : 0))
const hexToRgb = (hex: string): [number, number, number] => { const n = parseInt(hex.slice(1), 16); return [n >> 16 & 255, n >> 8 & 255, n & 255] }

export function WaterOrb({ percent, compacting, enabled, size, label, color, onPress }: WaterOrbProps) {
  const guard = useSelectionGuard()
  const canvas = useRef<HTMLCanvasElement>(null)
  // 每帧读最新属性，不触发 React 重渲染
  const latest = useRef({ occupancy: clamp(percent / 100), compacting, color: hexToRgb(color) })
  latest.current = { occupancy: clamp(percent / 100), compacting, color: hexToRgb(color) }

  useEffect(() => {
    const element = canvas.current!
    const c = element.getContext('2d')!
    const reduced = matchMedia('(prefers-reduced-motion: reduce)')
    const dpr = Math.min(devicePixelRatio || 1, 2)
    element.width = Math.round(size * dpr)
    element.height = Math.round(size * dpr)
    let level = latest.current.occupancy, requestedLevel = level, lastFill = level
    let t = 0, agitation = 0, accumulator = 0, previous = 0, frame = 0
    const fluid = new Fluid(level)
    const bubbles = new Bubbles()
    const texture = document.createElement('canvas')
    texture.width = texture.height = resolution
    const textureContext = texture.getContext('2d')!
    const pixels = textureContext.createImageData(resolution, resolution)
    const field = new Float32Array(resolution * resolution)
    const physicalField = new Float32Array(resolution * resolution)
    const blurred = new Float32Array(resolution * resolution)
    const splatRadius = 6.8

    const splat = (x0: number, y0: number, destination: Float32Array) => {
      const cx = (x0 + 1) * resolution / 2, cy = (y0 + 1) * resolution / 2
      for (let y = Math.max(0, Math.floor(cy - splatRadius)); y <= Math.min(resolution - 1, cy + splatRadius); y++) {
        for (let x = Math.max(0, Math.floor(cx - splatRadius)); x <= Math.min(resolution - 1, cx + splatRadius); x++) {
          const d = ((x - cx) ** 2 + (y - cy) ** 2) / (splatRadius * splatRadius)
          if (d < 1) destination[y * resolution + x] += (1 - d) ** 2
        }
      }
    }
    // 气泡预热：一打开就有气泡，而不是从空开始
    let bubblesReady = false

    function draw(now: number) {
      const dt = previous ? Math.min((now - previous) / 1000, .05) : 0
      previous = now
      const target = latest.current
      const still = reduced.matches
      if (still) { level = target.occupancy; requestedLevel = level; agitation = 0 }
      t += still ? 0 : dt
      if (!target.compacting) requestedLevel = target.occupancy
      // 翻滚平息后才回落水位，不跳到新水位
      if (!target.compacting && agitation < .18) {
        level += (requestedLevel - level) * (1 - Math.exp(-dt * 3))
        if (Math.abs(requestedLevel - level) < .003) {
          level = requestedLevel
          if (Math.abs(lastFill - level) > .002) { fluid.fill(level); lastFill = level }
        }
      }
      agitation = still ? 0 : agitation + ((target.compacting ? 1 : 0) - agitation) * (1 - Math.exp(-dt * (target.compacting ? 3 : 1.8)))
      const fluidBlend = Math.min(1, agitation * 2)
      // 只在压缩翻滚时跑物理；平时水面是解析波浪，省掉这部分开销
      if (!still && (target.compacting || agitation > .01)) {
        accumulator += dt
        let steps = 0
        while (accumulator >= 1 / 120 && steps < 6) { fluid.step(1 / 120, target.compacting); accumulator -= 1 / 120; steps++ }
      } else accumulator = 0

      c.setTransform(dpr, 0, 0, dpr, 0, 0)
      c.clearRect(0, 0, size, size)
      c.translate(size / 2, size / 2)
      c.scale(size * .465, size * .465)
      c.save()
      c.beginPath(); c.arc(0, 0, .99, 0, TAU); c.clip()
      // 卡通外壳：干净的浅色底和一圈描边，不做写实折射
      const shell = c.createRadialGradient(-.24, -.28, .12, .08, .12, 1.2)
      shell.addColorStop(0, 'rgba(255,255,255,.12)')
      shell.addColorStop(.6, 'rgba(231,243,255,.09)')
      shell.addColorStop(.84, 'rgba(149,188,228,.14)')
      shell.addColorStop(1, 'rgba(211,232,251,.34)')
      c.fillStyle = shell
      c.fillRect(-1, -1, 2, 2)

      // 翻滚时由粒子重建连续水面，不旋转一张画好的遮罩
      if (fluidBlend > 0) {
        field.fill(0)
        physicalField.fill(0)
        for (const particle of fluid.particles) { splat(particle.x, particle.y, physicalField); splat(particle.x, particle.y, field) }
        for (let y = 0; y < resolution; y++) for (let x = 0; x < resolution; x++) {
          let sum = 0
          for (let k = -2; k <= 2; k++) sum += field[y * resolution + Math.max(0, Math.min(resolution - 1, x + k))]
          blurred[y * resolution + x] = sum / 5
        }
        for (let y = 0; y < resolution; y++) for (let x = 0; x < resolution; x++) {
          let sum = 0
          for (let k = -2; k <= 2; k++) sum += blurred[Math.max(0, Math.min(resolution - 1, y + k)) * resolution + x]
          field[y * resolution + x] = sum / 5
        }
      }
      // 平时的海面：波峰波谷向前推进，占用越高浪越大越快
      const waveSpeed = .85 + level * 2.8
      const waveAmplitude = (.009 + .035 * level * level) * Math.min(1, level * 20, (1 - level) * 16)
      const surface = (x: number) => 1 - 2 * level + waveAmplitude * (
        Math.sin(x * 4.4 - t * waveSpeed) + .38 * Math.sin(x * 8.2 - t * waveSpeed * 1.36 + .8) + .15 * Math.sin(x * 13.1 + t * waveSpeed * .65))
      const data = pixels.data
      const [red, green, blue] = target.color
      const threshold = .7 - agitation * .2
      for (let y = 0; y < resolution; y++) for (let x = 0; x < resolution; x++) {
        const index = y * resolution + x, k = index * 4
        const nx = x / resolution * 2 - 1, ny = y / resolution * 2 - 1
        const sea = Math.max(0, Math.min(4, (ny - surface(nx)) / .016 + 1))
        const value = sea * (1 - fluidBlend) + field[index] * fluidBlend
        data[k] = red; data[k + 1] = green; data[k + 2] = blue
        data[k + 3] = Math.round(Math.max(0, Math.min(1, (value - threshold) * 7)) * 216)
      }
      textureContext.putImageData(pixels, 0, 0)
      c.imageSmoothingEnabled = true
      c.drawImage(texture, -1, -1, 2, 2)

      const submerged = (x: number, y: number) => {
        if (x * x + y * y > .96 * .96) return false
        if (fluidBlend < .5) return y > surface(x) + .006
        const ix = Math.floor((x + 1) * resolution / 2), iy = Math.floor((y + 1) * resolution / 2)
        return ix >= 0 && ix < resolution && iy >= 0 && iy < resolution && physicalField[iy * resolution + ix] > 1.1
      }
      const flow = (x: number, y: number): [number, number] => {
        let vx = 0, vy = 0, weight = 0
        for (const particle of fluid.particles) {
          const d = (particle.x - x) ** 2 + (particle.y - y) ** 2
          if (d < .035) { const w = .035 - d; vx += particle.vx * w; vy += particle.vy * w; weight += w }
        }
        return weight ? [vx / weight, vy / weight] : [0, 0]
      }
      if (!bubblesReady) { for (let i = 0; i < 240; i++) bubbles.step(1 / 60, level, submerged, () => [0, 0]); bubblesReady = true }
      if (!still) bubbles.step(dt, level, submerged, fluidBlend > 0 ? flow : () => [0, 0])
      c.save()
      if (fluidBlend < .5) {
        c.beginPath(); c.moveTo(-1.1, 1.1); c.lineTo(-1.1, surface(-1.1))
        for (let x = -1.1; x <= 1.11; x += .015) c.lineTo(x, surface(x))
        c.lineTo(1.1, 1.1); c.closePath(); c.clip()
      }
      // 水位回落期间不画气泡
      const showBubbles = target.compacting || level <= requestedLevel + .003
      for (const bubble of showBubbles ? bubbles.items : []) {
        const alpha = bubble.state === 'popping' ? Math.max(0, 1 - bubble.pop / .22) : Math.min(1, bubble.age / .18)
        c.globalAlpha = alpha
        c.beginPath(); c.arc(bubble.x, bubble.y, bubble.radius, 0, TAU)
        c.fillStyle = 'rgba(255,255,255,.62)'; c.fill()
      }
      c.globalAlpha = 1
      c.restore()
      // 柔和的球体光照：正面亮，右下边缘稍暗
      const volume = c.createRadialGradient(-.28, -.38, .08, .08, .12, 1.28)
      volume.addColorStop(0, 'rgba(255,255,255,.25)')
      volume.addColorStop(.42, 'rgba(255,255,255,.06)')
      volume.addColorStop(.66, 'rgba(178,218,255,.02)')
      volume.addColorStop(.86, 'rgba(167,213,255,.05)')
      volume.addColorStop(1, 'rgba(155,207,255,.08)')
      c.fillStyle = volume; c.fillRect(-1, -1, 2, 2)
      // 薄玻璃厚度
      const rim = c.createRadialGradient(0, 0, .73, 0, 0, 1.01)
      rim.addColorStop(0, 'rgba(255,255,255,0)')
      rim.addColorStop(.53, 'rgba(194,227,255,.03)')
      rim.addColorStop(.76, 'rgba(183,223,255,.06)')
      rim.addColorStop(.89, 'rgba(245,252,255,.78)')
      rim.addColorStop(1, 'rgba(172,218,255,.20)')
      c.fillStyle = rim; c.fillRect(-1, -1, 2, 2)
      const transmitted = c.createRadialGradient(.18, .84, 0, .18, .84, .48)
      transmitted.addColorStop(0, 'rgba(236,250,255,.30)')
      transmitted.addColorStop(1, 'rgba(236,250,255,0)')
      c.fillStyle = transmitted; c.fillRect(-1, -1, 2, 2)
      c.beginPath(); c.arc(-.23, -.84, .035, 0, TAU); c.fillStyle = '#fff'; c.fill()
      c.restore()
      c.beginPath(); c.arc(0, 0, .99, 0, TAU)
      c.strokeStyle = 'rgba(159,210,255,.30)'; c.lineWidth = .013; c.stroke()
      frame = requestAnimationFrame(draw)
    }
    // 窗口隐藏时停下，回来后接着画
    const resume = () => {
      cancelAnimationFrame(frame)
      previous = 0
      if (!document.hidden) frame = requestAnimationFrame(draw)
    }
    resume()
    document.addEventListener('visibilitychange', resume)
    return () => { cancelAnimationFrame(frame); document.removeEventListener('visibilitychange', resume); fluid.dispose() }
  }, [size])

  return <button type="button" className="water-orb" style={{ width: size, height: size }} aria-label={label} aria-disabled={!enabled}
    {...guard(() => { if (enabled) onPress?.() })}>
    <canvas ref={canvas} style={{ width: size, height: size }} aria-hidden="true" />
  </button>
}
