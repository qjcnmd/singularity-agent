/* 水里的气泡：从球底陆续冒出 → 上升并变大 → 到水面破掉。 */

export type Bubble = { x: number; y: number; vx: number; vy: number; age: number; base: number; radius: number; birthY: number; birthDepth: number; state: 'rising' | 'popping'; pop: number }
export type WetQuery = (x: number, y: number) => boolean
export type FlowQuery = (x: number, y: number) => [number, number]

export class Bubbles {
  items: Bubble[] = []
  private credit = 0

  private depth(x: number, y: number, wet: WetQuery) {
    let depth = 0
    for (let yy = y; yy > -.96; yy -= .015) {
      if (!wet(x, yy)) break
      depth += .015
    }
    return depth
  }

  step(dt: number, level: number, wet: WetQuery, flow: FlowQuery) {
    // 错开时间从水下冒出，不让已有气泡瞬移
    this.credit += dt * (.65 + level * level * 8)
    if (this.credit >= 1) {
      this.credit -= 1
      if (this.items.length < Math.round(4 + level * 12) && level > .025) this.spawn(wet)
    }
    for (const bubble of this.items) {
      if (bubble.state === 'popping') {
        bubble.pop += dt
        bubble.y += Math.min(-.05, bubble.vy) * dt
        continue
      }
      bubble.age += dt
      const depth = this.depth(bubble.x, bubble.y, wet)
      const progress = Math.max(0, Math.min(1, (bubble.birthY - bubble.y) / Math.max(.06, bubble.birthDepth)))
      // 等温膨胀（虚拟 10 cm 球径），再额外放大一些让上升过程更明显
      const birthPressure = 101325 + 1000 * 9.81 * bubble.birthDepth * .05
      const pressure = 101325 + 1000 * 9.81 * depth * .05
      bubble.radius = bubble.base * Math.cbrt(birthPressure / pressure) * (1 + 1.3 * progress)
      const [fx, fy] = flow(bubble.x, bubble.y)
      const rise = .12 + level * level * .65 + progress * .18
      const relax = 1 - Math.exp(-dt * 5)
      bubble.vx += (fx * .65 - bubble.vx) * relax
      bubble.vy += (fy * .65 - rise - bubble.vy) * relax
      bubble.x += bubble.vx * dt
      bubble.y += bubble.vy * dt
      const limit = .942 - bubble.radius, radius = Math.hypot(bubble.x, bubble.y)
      if (radius > limit) {
        const nx = bubble.x / radius, ny = bubble.y / radius
        bubble.x = nx * limit
        bubble.y = ny * limit
        const outward = bubble.vx * nx + bubble.vy * ny
        if (outward > 0) { bubble.vx -= outward * nx; bubble.vy -= outward * ny }
      }
      if (!wet(bubble.x, bubble.y) || bubble.age > 18) { bubble.state = 'popping'; bubble.pop = 0 }
    }
    // 气泡之间保持间距，不连成一串
    for (let i = 0; i < this.items.length; i++) {
      for (let j = i + 1; j < this.items.length; j++) {
        const a = this.items[i], b = this.items[j]
        if (a.state !== 'rising' || b.state !== 'rising') continue
        const dx = b.x - a.x, dy = b.y - a.y, distance = Math.hypot(dx, dy), min = a.radius + b.radius + .035
        if (distance >= min || distance < .0001) continue
        const push = (min - distance) * .5, nx = dx / distance, ny = dy / distance
        const ax = a.x - nx * push, ay = a.y - ny * push, bx = b.x + nx * push, by = b.y + ny * push
        if (wet(ax, ay) && wet(bx, by)) { a.x = ax; a.y = ay; b.x = bx; b.y = by }
      }
    }
    this.items = this.items.filter(bubble => bubble.pop < .22)
  }

  private spawn(wet: WetQuery) {
    for (let attempt = 0; attempt < 10; attempt++) {
      const x = (Math.random() * 2 - 1) * .68
      const y = Math.sqrt(.91 * .91 - x * x) - .025
      const depth = this.depth(x, y, wet)
      if (wet(x, y) && depth > .075 && !this.items.some(bubble => Math.hypot(bubble.x - x, bubble.y - y) < .16)) {
        const base = .019 + Math.random() * .004
        this.items.push({ x, y, vx: 0, vy: 0, age: 0, base, radius: base, birthY: y, birthDepth: depth, state: 'rising', pop: 0 })
        return
      }
    }
  }
}
