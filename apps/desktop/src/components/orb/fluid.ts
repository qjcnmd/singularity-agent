/// <reference path="../../../node_modules/liquidfun-wasm/dist/Box2DModule.d.ts" />
import factory from 'liquidfun-wasm/dist/es/Box2D.js'
import wasmUrl from 'liquidfun-wasm/dist/es/Box2D.wasm?url&inline'

/* 水球里的水：用未修改的 LiquidFun（Box2D 流体分支）WASM 做粒子水，这里只是适配层。
 * 压缩时转动球壁带动水翻滚；重力固定向下，不旋转重力场。 */

let engine: typeof Box2D & EmscriptenModule

/** 应用启动时调用一次，加载完成后才能创建 Fluid。 */
export async function initFluid() {
  // 内联字节使 Electron 的 file:// 页面无需发出 fetch，也无需放宽 connect-src。
  const wasmBinary = Uint8Array.from(atob(wasmUrl.slice(wasmUrl.indexOf(',') + 1)), character => character.charCodeAt(0))
  engine = await factory({ wasmBinary: wasmBinary.buffer })
}

export type Particle = { x: number; y: number; vx: number; vy: number }

export class Fluid {
  particles: Particle[] = []
  private world: Box2D.b2World
  private system!: Box2D.b2ParticleSystem
  private gravity: Box2D.b2Vec2
  private wall: Box2D.b2Body
  private shear: Box2D.b2Vec2
  private angular = 0
  private time = 0

  constructor(level: number) {
    const B = engine
    this.gravity = new B.b2Vec2(0, 9.81)
    this.world = new B.b2World(this.gravity)
    this.shear = new B.b2Vec2(0, 0)
    const bodyDef = new B.b2BodyDef()
    bodyDef.set_type(B.b2_kinematicBody)
    this.wall = this.world.CreateBody(bodyDef)
    B.destroy(bodyDef)
    // 64 段小方块围成圆形球壁
    const center = new B.b2Vec2(0, 0)
    for (let i = 0; i < 64; i++) {
      const angle = i * Math.PI * 2 / 64
      center.Set(Math.cos(angle) * 1.015, Math.sin(angle) * 1.015)
      const shape = new B.b2PolygonShape()
      shape.SetAsBox(.06, .04, center, angle + Math.PI / 2)
      this.wall.CreateFixture(shape, 0)
      B.destroy(shape)
    }
    B.destroy(center)
    this.fill(level)
  }

  /** 按水位高度重新铺满粒子（水位是高度占比，不是球体体积）。 */
  fill(level: number) {
    const B = engine
    if (this.system) this.world.DestroyParticleSystem(this.system)
    const systemDef = new B.b2ParticleSystemDef()
    systemDef.set_radius(.026)
    systemDef.set_dampingStrength(.12)
    systemDef.set_density(1)
    this.system = this.world.CreateParticleSystem(systemDef)
    B.destroy(systemDef)
    const particleDef = new B.b2ParticleDef()
    const position = new B.b2Vec2(0, 0)
    // 原生的水粒子压力求解，不是弹簧粒子
    particleDef.set_flags(B.b2_waterParticle)
    const spacing = .039
    let row = 0
    for (let y = .925; y > -.94; y -= spacing * .866) {
      for (let x = -.94 + (row % 2) * spacing * .5; x < .94; x += spacing) {
        if (x * x + y * y < .94 * .94 && y >= 1 - 2 * level) {
          position.Set(x, y)
          particleDef.set_position(position)
          this.system.CreateParticle(particleDef)
        }
      }
      row++
    }
    B.destroy(position)
    B.destroy(particleDef)
    this.read()
  }

  private read() {
    const count = this.system.GetParticleCount()
    const positions = engine.getPointer(this.system.GetPositionBuffer()) / 4
    const velocities = engine.getPointer(this.system.GetVelocityBuffer()) / 4
    const heap = engine.HEAPF32
    while (this.particles.length < count) this.particles.push({ x: 0, y: 0, vx: 0, vy: 0 })
    this.particles.length = count
    for (let i = 0; i < count; i++) {
      const particle = this.particles[i]
      particle.x = heap[positions + i * 2]
      particle.y = heap[positions + i * 2 + 1]
      particle.vx = heap[velocities + i * 2]
      particle.vy = heap[velocities + i * 2 + 1]
    }
  }

  step(dt: number, shaking: boolean) {
    this.time += dt
    const desired = shaking ? 5.4 + .18 * Math.sin(this.time * .42) : 0
    this.angular += (desired - this.angular) * (1 - Math.exp(-dt * 2.5))
    this.wall.SetAngularVelocity(this.angular)
    // 只在贴着球壁的薄层里施加切向力，中间的水自由下落、翻滚，而不是整体刚性转动
    const particleMass = .039 * .039
    for (let i = 0; i < this.particles.length; i++) {
      const particle = this.particles[i]
      const radius = Math.hypot(particle.x, particle.y)
      if (radius < .8) continue
      const weight = Math.min(1, (radius - .8) / .15)
      const tx = -particle.y / radius, ty = particle.x / radius
      const tangentVelocity = particle.vx * tx + particle.vy * ty
      const acceleration = (this.angular * radius - tangentVelocity) * weight * 11
      this.shear.Set(tx * acceleration * particleMass, ty * acceleration * particleMass)
      this.system.ParticleApplyForce(i, this.shear)
    }
    this.world.Step(dt, 6, 3, 3)
    this.read()
  }

  dispose() {
    engine.destroy(this.world)
    engine.destroy(this.gravity)
    engine.destroy(this.shear)
  }
}
