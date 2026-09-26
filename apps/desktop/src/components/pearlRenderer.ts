import { createOrbCanvas } from './orbCanvas'

/** 玻璃球体内粉白色的半透明丝带。 */
export function createPearlRenderer(canvas: HTMLCanvasElement) {
  const { ctx, size, dpr } = createOrbCanvas(canvas, 'Pearl')
  const radius = size * 0.435
  // 丝带形状与配色固定，运行时只变换两张纹理；渐变与路径不再每帧重建。
  const ribbons = [0, 1].map(index => {
    const sprite = document.createElement('canvas')
    sprite.width = canvas.width
    sprite.height = canvas.height
    const brush = sprite.getContext('2d')
    if (!brush) throw new Error('Pearl orb texture requires a 2D canvas context')
    brush.setTransform(dpr, 0, 0, dpr, size / 2 * dpr, size / 2 * dpr)
    const ribbon = brush.createLinearGradient(-34, -30, 28, 38)
    ribbon.addColorStop(0, index % 2 ? '#fffaffd9' : '#ff81bba8')
    ribbon.addColorStop(0.45, '#ffd7ecbf')
    ribbon.addColorStop(1, '#d8549b1a')
    brush.fillStyle = ribbon
    brush.beginPath()
    brush.moveTo(-8, 10)
    brush.bezierCurveTo(-55, -8, -35, -50, 2, -38)
    brush.bezierCurveTo(38, -30, 31, 14, 22, 40)
    brush.bezierCurveTo(18, 15, 3, 15, -8, 10)
    brush.fill()
    return sprite
  })
  const shell = ctx.createRadialGradient(44, 40, 0, 64, 64, radius)
  shell.addColorStop(0, '#fff9fd')
  shell.addColorStop(0.45, '#f8e5ef')
  shell.addColorStop(0.83, '#ffd8eb')
  shell.addColorStop(1, '#ffc2df99')
  const core = ctx.createRadialGradient(59, 59, 0, 61, 61, 29)
  core.addColorStop(0, '#ffffff')
  core.addColorStop(0.25, '#fffffff5')
  core.addColorStop(0.6, '#fff5f9a0')
  core.addColorStop(1, '#ffffff00')
  const highlight = ctx.createRadialGradient(44, 30, 0, 44, 30, 27)
  highlight.addColorStop(0, '#ffffff8c')
  highlight.addColorStop(1, '#ffffff00')
  let time = 0
  return (elapsed: number) => {
    time += elapsed
    ctx.clearRect(0, 0, size, size)
    ctx.save()
    ctx.translate(size / 2, size / 2)
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
      ctx.drawImage(ribbons[index % 2], -size / 2, -size / 2, size, size)
      ctx.restore()
    }
    ctx.restore()
    ctx.fillStyle = core
    ctx.fillRect(0, 0, size, size)
    ctx.fillStyle = highlight
    ctx.beginPath()
    ctx.arc(64, 64, radius, 0, Math.PI * 2)
    ctx.fill()
  }
}
