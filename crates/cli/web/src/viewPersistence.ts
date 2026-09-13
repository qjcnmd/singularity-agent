// 浏览器视图的存储、草稿迁移和默认值；运行态由 Store 独立维护。
import type { ViewportAnchor } from './protocol'

export const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following', anchorItemId: null, offset: 0,
})

export const messageFontSize = { min: 12, max: 24, default: 16 }
export function normalizeMessageFontSize(value: number): number {
  return Number.isFinite(value) ? Math.min(messageFontSize.max, Math.max(messageFontSize.min, Math.round(value))) : messageFontSize.default
}

export const storageKey = 'singularity.workbench.view.v1'
export const draftStoragePrefix = `${storageKey}:draft:`


export interface PersistedView {
  version: 1
  theme: 'light' | 'dark'
  messageFontSize: number
  selectedWorkspaceId: string | null
  selectedSessionId: string | null
  drafts: Record<string, string>
  legacyDraftIds: string[]
  sidebarWidth: number
  sidebarCollapsed: boolean
  sidebarView: { collapsed: string[] }
  trajectoryOpen: boolean
  workspaceAppearance: Record<string, WorkspaceAppearance>
  viewportAnchors: Record<string, ViewportAnchor>
}

export interface WorkspaceAppearance {
  icon: string
  color: string
}

export function loadPersisted(): PersistedView {
  const fallback: PersistedView = {
    version: 1,
    theme: 'light',
    messageFontSize: messageFontSize.default,
    selectedWorkspaceId: null,
    selectedSessionId: null,
    drafts: {},
    legacyDraftIds: [],
    sidebarWidth: 280,
    sidebarCollapsed: false,
    sidebarView: { collapsed: [] },
    trajectoryOpen: false,
    workspaceAppearance: {},
    viewportAnchors: {},
  }
  try {
    const stored = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as Partial<PersistedView> | null
    const value = stored?.version === 1 ? stored : fallback
    const drafts: Record<string, string> = { ...value.drafts }
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index)
      if (key?.startsWith(draftStoragePrefix)) drafts[key.slice(draftStoragePrefix.length)] = localStorage.getItem(key) ?? ''
    }
    return {
      ...fallback,
      theme: value.theme === 'dark' ? 'dark' : 'light',
      messageFontSize: normalizeMessageFontSize(value.messageFontSize ?? messageFontSize.default),
      selectedWorkspaceId: value.selectedWorkspaceId ?? null,
      selectedSessionId: value.selectedSessionId ?? null,
      drafts,
      legacyDraftIds: Object.keys(value.drafts ?? {}),
      sidebarWidth: clampSidebarWidth(value.sidebarWidth ?? 280),
      sidebarCollapsed: value.sidebarCollapsed ?? false,
      sidebarView: { collapsed: value.sidebarView?.collapsed ?? [] },
      trajectoryOpen: value.trajectoryOpen ?? false,
      workspaceAppearance: value.workspaceAppearance ?? {},
      viewportAnchors: value.viewportAnchors ?? {},
    }
  } catch {
    return fallback
  }
}

export function persistView(value: PersistedView): void {
  const { version, theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, sidebarView, trajectoryOpen, workspaceAppearance, viewportAnchors, drafts } = value
  // 旧容器内的草稿迁入独立键后才覆盖容器，写入失败时原副本仍在。
  for (const id of value.legacyDraftIds) {
    if (localStorage.getItem(draftStoragePrefix + id) === null) localStorage.setItem(draftStoragePrefix + id, drafts[id] ?? '')
  }
  localStorage.setItem(storageKey, JSON.stringify({ version, theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, sidebarView, trajectoryOpen, workspaceAppearance, viewportAnchors }))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
