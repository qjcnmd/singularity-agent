// 会话气泡配色：从调色盘取色，app.tsx 把结果写成根元素上的 CSS 变量。
// 我的气泡用实色；助手气泡用同色的浅底，“灰黑”表示白底细边框。浅底用 color-mix 叠在画布色上，深色主题自动变暗。
import type { PersistedView } from './viewPersistence'

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

export const defaultBubbleColors = { assistantBubble: 'neutral', userBubble: 'blue' } as const

export function bubbleVariables(view: Pick<PersistedView, 'assistantBubble' | 'userBubble'>): Record<string, string> {
  const assistant = paletteColor(view.assistantBubble) ?? paletteColor(defaultBubbleColors.assistantBubble)!
  const user = paletteColor(view.userBubble) ?? paletteColor(defaultBubbleColors.userBubble)!
  const neutral = assistant.id === 'neutral'
  const userNeutral = user.id === 'neutral'
  return {
    '--accent': neutral ? 'var(--default-accent)' : assistant.solid,
    '--accent-soft': neutral ? 'var(--default-accent-soft)' : `color-mix(in srgb, ${assistant.solid} 10%, var(--canvas))`,
    '--ink-on-accent': neutral ? '#fff' : assistant.ink,
    '--assistant-bubble-bg': neutral ? 'var(--surface)' : `color-mix(in srgb, ${assistant.solid} 10%, var(--surface))`,
    '--assistant-bubble-border': neutral ? 'var(--bubble-line)' : `color-mix(in srgb, ${assistant.solid} 22%, var(--surface))`,
    '--assistant-bubble-ink': 'var(--message-ink)',
    '--assistant-selection-bg': `color-mix(in srgb, ${neutral ? 'var(--accent)' : assistant.solid} 10%, var(--canvas))`,
    /* 用户泡选 neutral 时和助手一致：白底细边框。其它色还是实色底+同色系描边。 */
    '--user-bubble-bg': userNeutral ? 'var(--surface)' : user.solid,
    '--user-bubble-border': userNeutral ? 'var(--bubble-line)' : user.solid,
    '--user-bubble-ink': userNeutral ? 'var(--message-ink)' : user.ink,
  }
}
