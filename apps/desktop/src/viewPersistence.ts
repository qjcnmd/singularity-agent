// 桌面视图及默认值；运行态由 Store 独立维护。
import type { ViewportAnchor } from './protocol'
import type { SessionView } from './execution'
import { buildTimeline } from './timeline'

/** 找到所属回合后，缺失条目不可能藏在更早的页里。 */
export function anchorLocation(session: SessionView | null, anchor: ViewportAnchor | undefined): 'found' | 'older' | 'missing' {
  if (anchor?.mode !== 'anchored') return 'found'
  if (buildTimeline(session).some(item => item.key === anchor.anchorItemId)) return 'found'
  if (session === null || session.nextCursor === null || anchor.turnId === undefined
    || [...session.facts.history, ...session.facts.active].some(turn => turn.id === anchor.turnId)) return 'missing'
  return 'older'
}

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
  trajectoryWidth: number
  sidebarView: { collapsed: string[] }
  trajectoryOpen: boolean
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
    trajectoryWidth: 280,
    sidebarView: { collapsed: [] },
    trajectoryOpen: false,
    workspaceAppearance: {},
    viewportAnchors: {},
  }
  try {
    const stored = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as PersistedView | null
    return stored === null ? fallback : { ...fallback, ...stored }
  } catch {
    return fallback
  }
}

export function persistView(value: PersistedView): void {
  const { theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, trajectoryWidth, sidebarView, trajectoryOpen, workspaceAppearance, viewportAnchors } = value
  const view: PersistedView = { theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, trajectoryWidth, sidebarView, trajectoryOpen, workspaceAppearance, viewportAnchors }
  localStorage.setItem(storageKey, JSON.stringify(view))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
