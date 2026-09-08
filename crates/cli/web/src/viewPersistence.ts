// 浏览器视图的存储、草稿迁移和默认值；运行态由 Store 独立维护。
import type { ViewportAnchor } from './protocol'

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
  sidebarWidth: number
  sidebarCollapsed: boolean
  sidebarView: { grouping: 'workspace' | 'flat'; order: 'manual' | 'updated'; collapsed: string[]; sessionOrder: string[] }
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
    sidebarWidth: 280,
    sidebarCollapsed: false,
    sidebarView: { grouping: 'workspace', order: 'updated', collapsed: [], sessionOrder: [] },
    trajectoryOpen: false,
    workspaceAppearance: {},
    viewportAnchors: {},
  }
  try {
    const value = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as Partial<PersistedView> | null
    if (value?.version !== 1) return fallback
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
      sidebarWidth: clampSidebarWidth(value.sidebarWidth ?? 280),
      sidebarCollapsed: value.sidebarCollapsed ?? false,
      sidebarView: value.sidebarView ?? fallback.sidebarView,
      trajectoryOpen: value.trajectoryOpen ?? false,
      workspaceAppearance: value.workspaceAppearance ?? {},
      viewportAnchors: value.viewportAnchors ?? {},
    }
  } catch {
    return fallback
  }
}

export function persistView(state: PersistedView): void {
  // 覆盖旧容器前迁移草稿；写入失败时保留原容器，避免丢失唯一副本。
  const previous = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as Partial<PersistedView> | null
  if (previous?.version === 1) {
    for (const [id, text] of Object.entries(previous.drafts ?? {})) {
      if (localStorage.getItem(draftStoragePrefix + id) === null) localStorage.setItem(draftStoragePrefix + id, text)
    }
  }
  const view: Omit<PersistedView, 'drafts'> = {
    version: 1,
    theme: state.theme,
    messageFontSize: state.messageFontSize,
    selectedWorkspaceId: state.selectedWorkspaceId,
    selectedSessionId: state.selectedSessionId,
    sidebarWidth: state.sidebarWidth,
    sidebarCollapsed: state.sidebarCollapsed,
    sidebarView: state.sidebarView,
    trajectoryOpen: state.trajectoryOpen,
    workspaceAppearance: state.workspaceAppearance,
    viewportAnchors: state.viewportAnchors,
  }
  localStorage.setItem(storageKey, JSON.stringify(view))
}


export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
