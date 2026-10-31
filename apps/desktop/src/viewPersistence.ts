// 持久保存在 localStorage 中的界面偏好（侧栏宽度、主题、字号、阅读锚点等）及默认值。
import type { ViewportAnchor } from './protocol'

export const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following', anchorItemId: null, offset: 0,
})

export const messageFontSize = { min: 12, max: 24, default: 14.5 }
export function normalizeMessageFontSize(value: number): number {
  return Number.isFinite(value) ? Math.min(messageFontSize.max, Math.max(messageFontSize.min, value)) : messageFontSize.default
}

const storageKey = 'singularity.app.view.v1'


export interface PersistedView {
  theme: 'light' | 'dark'
  messageFontSize: number
  /** 会话气泡配色，取调色盘颜色的 id（bubbleColors.ts）。 */
  assistantBubble: string
  userBubble: string
  selectedWorkspaceRoot: string | null
  selectedSessionId: string | null
  sidebarWidth: number
  filePanelWidth: number
  sidebarCollapsed: boolean
  sidebarView: { collapsed: string[] }
  backgroundOpen: boolean
  viewportAnchors: Record<string, ViewportAnchor>
}

export function loadPersisted(): PersistedView {
  const fallback: PersistedView = {
    theme: 'light',
    messageFontSize: messageFontSize.default,
    assistantBubble: 'neutral',
    userBubble: 'blue',
    selectedWorkspaceRoot: null,
    selectedSessionId: null,
    sidebarWidth: 268,
    filePanelWidth: 480,
    sidebarCollapsed: false,
    sidebarView: { collapsed: [] },
    backgroundOpen: false,
    viewportAnchors: {},
  }
  try {
    const stored = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as PersistedView | null
    // 新增的偏好在旧记录里没有，补上默认值。
    return stored ? { ...fallback, ...stored } : fallback
  } catch {
    return fallback
  }
}

export function persistView(value: PersistedView): void {
  const { theme, messageFontSize, assistantBubble, userBubble, selectedWorkspaceRoot, selectedSessionId, sidebarWidth, filePanelWidth, sidebarCollapsed, sidebarView, backgroundOpen, viewportAnchors } = value
  const view: PersistedView = { theme, messageFontSize, assistantBubble, userBubble, selectedWorkspaceRoot, selectedSessionId, sidebarWidth, filePanelWidth, sidebarCollapsed, sidebarView, backgroundOpen, viewportAnchors }
  localStorage.setItem(storageKey, JSON.stringify(view))
}

export function clampFilePanelWidth(value: number): number {
  return Math.min(Math.max(320, innerWidth - 640), Math.max(320, value))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
