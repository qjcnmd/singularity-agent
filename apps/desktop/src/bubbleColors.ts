// 会话气泡配色：从调色盘取色，app.tsx 把结果写成根元素上的 CSS 变量。
// 我的气泡用实色；助手气泡用同色的浅底，“灰黑”表示白底细边框。浅底用 color-mix 叠在画布色上，深色主题自动变暗。
import { paletteColor } from './palette'
import type { PersistedView } from './viewPersistence'

export const defaultBubbleColors = { assistantBubble: 'neutral', userBubble: 'blue' } as const

export function bubbleVariables(view: Pick<PersistedView, 'assistantBubble' | 'userBubble'>): Record<string, string> {
  const assistant = paletteColor(view.assistantBubble) ?? paletteColor(defaultBubbleColors.assistantBubble)!
  const user = paletteColor(view.userBubble) ?? paletteColor(defaultBubbleColors.userBubble)!
  const neutral = assistant.id === 'neutral'
  return {
    '--accent': neutral ? 'var(--default-accent)' : assistant.solid,
    '--accent-soft': neutral ? 'var(--default-accent-soft)' : `color-mix(in srgb, ${assistant.solid} 10%, var(--canvas))`,
    '--ink-on-accent': neutral ? '#fff' : assistant.ink,
    '--assistant-bubble-bg': neutral ? 'var(--canvas)' : `color-mix(in srgb, ${assistant.solid} 10%, var(--canvas))`,
    '--assistant-bubble-border': neutral ? 'var(--bubble-line)' : `color-mix(in srgb, ${assistant.solid} 22%, var(--canvas))`,
    '--assistant-bubble-ink': 'var(--ink)',
    '--assistant-selection-bg': `color-mix(in srgb, ${neutral ? 'var(--accent)' : assistant.solid} 10%, var(--canvas))`,
    '--user-bubble-bg': user.solid, '--user-bubble-border': user.solid, '--user-bubble-ink': user.ink,
  }
}
