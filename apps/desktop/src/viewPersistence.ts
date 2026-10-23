// 桌面视图及默认值；运行态由 Store 独立维护。
import type { ViewportAnchor } from './protocol'

export const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following', anchorItemId: null, offset: 0,
})

export const messageFontSize = { min: 12, max: 24, default: 16 }
export function normalizeMessageFontSize(value: number): number {
  return Number.isFinite(value) ? Math.min(messageFontSize.max, Math.max(messageFontSize.min, Math.round(value))) : messageFontSize.default
}

const storageKey = 'singularity.app.view.v1'


export interface PersistedView {
  theme: 'light' | 'dark'
  messageFontSize: number
  selectedWorkspaceId: string | null
  selectedSessionId: string | null
  sidebarWidth: number
  sidebarCollapsed: boolean
  backgroundWidth: number
  sidebarView: { collapsed: string[] }
  backgroundOpen: boolean
  workspaceAppearance: Record<string, WorkspaceAppearance>
  viewportAnchors: Record<string, ViewportAnchor>
}

export interface WorkspaceAppearance {
  color: string
}

export function loadPersisted(): PersistedView {
  const fallback: PersistedView = {
    theme: 'light',
    messageFontSize: messageFontSize.default,
    selectedWorkspaceId: null,
    selectedSessionId: null,
    sidebarWidth: 280,
    sidebarCollapsed: false,
    backgroundWidth: 280,
    sidebarView: { collapsed: [] },
    backgroundOpen: false,
    workspaceAppearance: {},
    viewportAnchors: {},
  }
  try {
    const stored = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as PersistedView | null
    return stored ?? fallback
  } catch {
    return fallback
  }
}

export function persistView(value: PersistedView): void {
  const { theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, backgroundWidth, sidebarView, backgroundOpen, workspaceAppearance, viewportAnchors } = value
  const view: PersistedView = { theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, backgroundWidth, sidebarView, backgroundOpen, workspaceAppearance, viewportAnchors }
  localStorage.setItem(storageKey, JSON.stringify(view))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
