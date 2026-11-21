import { defaultAccentColor, resolveAccentId } from './bubbleColors'
import { VIEW_STORAGE_KEY } from './identity'
import type { ViewportAnchor } from './protocol'

export const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following', anchorItemId: null, offset: 0,
})

export const messageFontSize = { min: 12, max: 24, default: 14.5 }
export function normalizeMessageFontSize(value: number): number {
  return Number.isFinite(value) ? Math.min(messageFontSize.max, Math.max(messageFontSize.min, value)) : messageFontSize.default
}

const storageKey = VIEW_STORAGE_KEY


export interface PersistedView {
  theme: 'light' | 'dark'
  messageFontSize: number
  /** 全局强调色，取调色盘颜色 id（bubbleColors.ts）。 */
  accentColor: string
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
    accentColor: defaultAccentColor,
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
    const stored = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as (Partial<PersistedView> & { assistantBubble?: string }) | null
    if (!stored) return fallback
    const accentColor = resolveAccentId(stored.accentColor ?? stored.assistantBubble)
    return { ...fallback, ...stored, accentColor }
  } catch {
    return fallback
  }
}

export function persistView(value: PersistedView): void {
  const { theme, messageFontSize, accentColor, selectedWorkspaceRoot, selectedSessionId, sidebarWidth, filePanelWidth, sidebarCollapsed, sidebarView, backgroundOpen, viewportAnchors } = value
  const view: PersistedView = { theme, messageFontSize, accentColor, selectedWorkspaceRoot, selectedSessionId, sidebarWidth, filePanelWidth, sidebarCollapsed, sidebarView, backgroundOpen, viewportAnchors }
  localStorage.setItem(storageKey, JSON.stringify(view))
}

export function clampFilePanelWidth(value: number): number {
  return Math.min(Math.max(320, innerWidth - 640), Math.max(320, value))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
