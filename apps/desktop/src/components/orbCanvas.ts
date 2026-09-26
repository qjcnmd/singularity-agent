/** 活动球体共用逻辑坐标和像素密度，CSS 决定最终显示尺寸。 */
export function createOrbCanvas(canvas: HTMLCanvasElement, name: string) {
  const ctx = canvas.getContext('2d')
  if (!ctx) throw new Error(`${name} orb requires a 2D canvas context`)
  const size = 128
  // 逻辑坐标保持不变，光栅尺寸对应实际显示像素，避免小球使用整幅 128px 画布。
  const pixels = Math.max(1, Math.ceil(canvas.getBoundingClientRect().width * window.devicePixelRatio))
  const dpr = pixels / size
  canvas.width = pixels
  canvas.height = pixels
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  return { ctx, size, dpr }
}
