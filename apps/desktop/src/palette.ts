// 消息气泡调色盘的 9 种颜色。

export interface PaletteColor {
  id: string
  label: string
  /** 实色：调色盘上的球、我的气泡底色。 */
  solid: string
  /** 实色底上的文字颜色。 */
  ink: string
}

export const palette: readonly PaletteColor[] = [
  { id: 'neutral', label: '灰黑', solid: '#52525b', ink: '#ffffff' },
  { id: 'red', label: '红色', solid: '#ef4444', ink: '#ffffff' },
  { id: 'orange', label: '橙色', solid: '#f97316', ink: '#ffffff' },
  { id: 'yellow', label: '黄色', solid: '#eab308', ink: '#3d2c00' },
  { id: 'green', label: '绿色', solid: '#22a447', ink: '#ffffff' },
  { id: 'teal', label: '青色', solid: '#14b8a6', ink: '#ffffff' },
  { id: 'blue', label: '蓝色', solid: '#1783ff', ink: '#ffffff' },
  { id: 'purple', label: '紫色', solid: '#8b5cf6', ink: '#ffffff' },
  { id: 'pink', label: '粉色', solid: '#ec72ad', ink: '#ffffff' },
]

/** 找不到时（比如旧版保存的值）返回 undefined，由调用方决定默认色。 */
export function paletteColor(id: string): PaletteColor | undefined {
  return palette.find(color => color.id === id)
}
