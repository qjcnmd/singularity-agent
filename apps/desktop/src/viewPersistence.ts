// 浏览器视图、分任务草稿键和默认值；运行态由 Store 独立维护。
import type { ViewportAnchor } from './protocol'

export const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following', anchorItemId: null, offset: 0,
})

export const messageFontSize = { min: 12, max: 24, default: 16 }
export function normalizeMessageFontSize(value: number): number {
  return Number.isFinite(value) ? Math.min(messageFontSize.max, Math.max(messageFontSize.min, Math.round(value))) : messageFontSize.default
}

const storageKey = 'singularity.app.view.v1'
/** 草稿存储键前缀只在本模块使用：键组装与写入由 persistDraft 独占，调用方不拼键。 */
const draftStoragePrefix = `${storageKey}:draft:`


export interface PersistedView {
  theme: 'light' | 'dark'
  messageFontSize: number
  selectedWorkspaceId: string | null
  selectedSessionId: string | null
  drafts: Record<string, string>
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
    theme: 'light',
    messageFontSize: messageFontSize.default,
    selectedWorkspaceId: null,
    selectedSessionId: null,
    drafts: {},
    sidebarWidth: 280,
    sidebarCollapsed: false,
    sidebarView: { collapsed: [] },
    trajectoryOpen: false,
    workspaceAppearance: {},
    viewportAnchors: {},
  }
  try {
    const stored = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as Partial<PersistedView> | null
    const value = stored ?? fallback
    const drafts: Record<string, string> = {}
    for (let index = localStorage.length - 1; index >= 0; index -= 1) {
      const key = localStorage.key(index)
      if (!key?.startsWith(draftStoragePrefix)) continue
      const text = localStorage.getItem(key) ?? ''
      if (text === '') localStorage.removeItem(key)
      else drafts[key.slice(draftStoragePrefix.length)] = text
    }
    return {
      ...fallback,
      theme: value.theme === 'dark' ? 'dark' : 'light',
      messageFontSize: normalizeMessageFontSize(value.messageFontSize ?? messageFontSize.default),
      selectedWorkspaceId: value.selectedWorkspaceId ?? null,
      selectedSessionId: value.selectedSessionId ?? null,
      drafts,
      sidebarWidth: clampSidebarWidth(value.sidebarWidth ?? fallback.sidebarWidth),
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

/** 写入单条草稿；空草稿不占存储键。失败交给 Store 提示，非空内容原样保留。 */
export function persistDraft(id: string, text: string): void {
  if (text === '') localStorage.removeItem(draftStoragePrefix + id)
  else localStorage.setItem(draftStoragePrefix + id, text)
}

export function persistView(value: PersistedView): void {
  const { theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, sidebarView, trajectoryOpen, workspaceAppearance, viewportAnchors } = value
  const view: Omit<PersistedView, 'drafts'> = { theme, messageFontSize, selectedWorkspaceId, selectedSessionId, sidebarWidth, sidebarCollapsed, sidebarView, trajectoryOpen, workspaceAppearance, viewportAnchors }
  localStorage.setItem(storageKey, JSON.stringify(view))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
