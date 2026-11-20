import type { PersistedView } from './viewPersistence'

export interface PaletteColor {
  id: string
  label: string
  solid: string
  ink: string
}

export const palette: readonly PaletteColor[] = [
  { id: 'blue', label: '海蓝', solid: '#1783ff', ink: '#ffffff' },
  { id: 'purple', label: '靛紫', solid: '#8b5cf6', ink: '#ffffff' },
  { id: 'pink', label: '樱粉', solid: '#ec72ad', ink: '#ffffff' },
  { id: 'red', label: '绯红', solid: '#ef4444', ink: '#ffffff' },
  { id: 'orange', label: '暖橙', solid: '#f97316', ink: '#ffffff' },
  { id: 'yellow', label: '琥珀黄', solid: '#eab308', ink: '#3d2c00' },
  { id: 'green', label: '翠绿', solid: '#22a447', ink: '#ffffff' },
  { id: 'teal', label: '青碧', solid: '#14b8a6', ink: '#ffffff' },
  { id: 'slate', label: '石墨灰', solid: '#52525b', ink: '#ffffff' },
]

/** 找不到时（比如旧版保存的 neutral）回退到默认强调色。 */
export function resolveAccentId(id: string | undefined): string {
  return palette.some(color => color.id === id) ? id! : defaultAccentColor
}

export const defaultAccentColor = 'blue' as const

export function bubbleVariables(view: Pick<PersistedView, 'accentColor'>): Record<string, string> {
  const chosen = palette.find(color => color.id === view.accentColor) ?? palette[0]
  return {
    '--accent': chosen.solid,
    '--accent-soft': `color-mix(in srgb, ${chosen.solid} 11%, var(--canvas))`,
    '--ink-on-accent': chosen.ink,
    '--assistant-bubble-bg': `color-mix(in srgb, ${chosen.solid} 10%, var(--surface))`,
    '--user-bubble-bg': 'var(--accent)',
    '--user-bubble-ink': 'var(--ink-on-accent)',
  }
}
