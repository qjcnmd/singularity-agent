/** 活动球体共用逻辑坐标和像素密度，CSS 决定最终显示尺寸。 */
export function createOrbCanvas(canvas: HTMLCanvasElement, name: string) {
  const ctx = canvas.getContext('2d')
  if (!ctx) throw new Error(`${name} orb requires a 2D canvas context`)
  const size = 128
  const dpr = Math.min(window.devicePixelRatio || 1, 2)
  canvas.width = size * dpr
  canvas.height = size * dpr
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  return { ctx, size, dpr }
}
