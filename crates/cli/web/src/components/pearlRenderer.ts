import { createOrbCanvas } from './orbCanvas'

/** 玻璃球体内粉白色的半透明丝带。 */
export function createPearlRenderer(canvas: HTMLCanvasElement) {
  const { ctx, size } = createOrbCanvas(canvas, 'Pearl')
  const radius = size * 0.435
  let time = 0
  return (elapsed: number) => {
    time += elapsed
    ctx.clearRect(0, 0, size, size)
    ctx.save()
    ctx.translate(size / 2, size / 2)
    const shell = ctx.createRadialGradient(-20, -24, 0, 0, 0, radius)
    shell.addColorStop(0, '#fff9fd')
    shell.addColorStop(0.45, '#f8e5ef')
    shell.addColorStop(0.83, '#ffd8eb')
    shell.addColorStop(1, '#ffc2df99')
    ctx.fillStyle = shell
    ctx.beginPath()
    ctx.arc(0, 0, radius, 0, Math.PI * 2)
    ctx.fill()
    ctx.clip()
    ctx.rotate(time * 0.07)
    for (let index = 0; index < 5; index++) {
      ctx.save()
      ctx.rotate(index * Math.PI * 2 / 5 + Math.sin(time * 0.12 + index) * 0.18)
      ctx.scale(1, 0.82 + Math.sin(time * 0.16 + index) * 0.12)
      const ribbon = ctx.createLinearGradient(-34, -30, 28, 38)
      ribbon.addColorStop(0, index % 2 ? '#fffaffd9' : '#ff81bba8')
      ribbon.addColorStop(0.45, '#ffd7ecbf')
      ribbon.addColorStop(1, '#d8549b1a')
      ctx.fillStyle = ribbon
      ctx.beginPath()
      ctx.moveTo(-8, 10)
      ctx.bezierCurveTo(-55, -8, -35, -50, 2, -38)
      ctx.bezierCurveTo(38, -30, 31, 14, 22, 40)
      ctx.bezierCurveTo(18, 15, 3, 15, -8, 10)
      ctx.fill()
      ctx.restore()
    }
    ctx.restore()
    const core = ctx.createRadialGradient(59, 59, 0, 61, 61, 29)
    core.addColorStop(0, '#ffffff')
    core.addColorStop(0.25, '#fffffff5')
    core.addColorStop(0.6, '#fff5f9a0')
    core.addColorStop(1, '#ffffff00')
    ctx.fillStyle = core
    ctx.fillRect(0, 0, size, size)
    const highlight = ctx.createRadialGradient(44, 30, 0, 44, 30, 27)
    highlight.addColorStop(0, '#ffffff8c')
    highlight.addColorStop(1, '#ffffff00')
    ctx.fillStyle = highlight
    ctx.beginPath()
    ctx.arc(64, 64, radius, 0, Math.PI * 2)
    ctx.fill()
  }
}
