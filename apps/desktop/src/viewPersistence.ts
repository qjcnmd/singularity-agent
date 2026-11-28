import { defaultAccentColor, resolveAccentId } from './bubbleColors'
import { VIEW_STORAGE_KEY } from './identity'
import type { ModelSelection, ViewportAnchor } from './protocol'

export const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following', anchorItemId: null, offset: 0,
})

export const messageFontSize = { min: 12, max: 24, default: 14.5 }
export function normalizeMessageFontSize(value: number): number {
  return Number.isFinite(value) ? Math.min(messageFontSize.max, Math.max(messageFontSize.min, value)) : messageFontSize.default
}


export interface PersistedView {
  theme: 'light' | 'dark'
  messageFontSize: number
  accentColor: string
  selectedWorkspaceRoot: string | null
  selectedSessionId: string | null
  lastModelSelection: ModelSelection | null
  sidebarWidth: number
  filePanelWidth: number
  sidebarCollapsed: boolean
  sidebarView: { collapsed: string[] }
  backgroundOpen: boolean
  stepsDefaultExpanded: boolean
  viewportAnchors: Record<string, ViewportAnchor>
}

function parseModelSelection(value: unknown): ModelSelection | null {
  if (!value || typeof value !== 'object') return null
  const { provider, model, reasoning } = value as Record<string, unknown>
  if (typeof provider !== 'string' || typeof model !== 'string') return null
  return { provider, model, reasoning: typeof reasoning === 'string' ? reasoning : null }
}

export function loadPersisted(): PersistedView {
  const fallback: PersistedView = {
    theme: 'light',
    messageFontSize: messageFontSize.default,
    accentColor: defaultAccentColor,
    selectedWorkspaceRoot: null,
    selectedSessionId: null,
    lastModelSelection: null,
    sidebarWidth: 268,
    filePanelWidth: 480,
    sidebarCollapsed: false,
    sidebarView: { collapsed: [] },
    backgroundOpen: false,
    stepsDefaultExpanded: true,
    viewportAnchors: {},
  }
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(VIEW_STORAGE_KEY) ?? 'null')
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return fallback
    const stored = raw as Partial<PersistedView>
    return {
      theme: stored.theme === 'dark' || stored.theme === 'light' ? stored.theme : fallback.theme,
      messageFontSize: typeof stored.messageFontSize === 'number'
        ? normalizeMessageFontSize(stored.messageFontSize)
        : fallback.messageFontSize,
      accentColor: resolveAccentId(typeof stored.accentColor === 'string' ? stored.accentColor : undefined),
      selectedWorkspaceRoot: typeof stored.selectedWorkspaceRoot === 'string' || stored.selectedWorkspaceRoot === null
        ? stored.selectedWorkspaceRoot
        : fallback.selectedWorkspaceRoot,
      selectedSessionId: typeof stored.selectedSessionId === 'string' || stored.selectedSessionId === null
        ? stored.selectedSessionId
        : fallback.selectedSessionId,
      lastModelSelection: parseModelSelection(stored.lastModelSelection),
      sidebarWidth: typeof stored.sidebarWidth === 'number' && Number.isFinite(stored.sidebarWidth)
        ? clampSidebarWidth(stored.sidebarWidth)
        : fallback.sidebarWidth,
      filePanelWidth: typeof stored.filePanelWidth === 'number' && Number.isFinite(stored.filePanelWidth)
        ? clampFilePanelWidth(stored.filePanelWidth)
        : fallback.filePanelWidth,
      sidebarCollapsed: typeof stored.sidebarCollapsed === 'boolean' ? stored.sidebarCollapsed : fallback.sidebarCollapsed,
      sidebarView: stored.sidebarView && typeof stored.sidebarView === 'object' && Array.isArray(stored.sidebarView.collapsed)
        ? { collapsed: stored.sidebarView.collapsed.filter((item): item is string => typeof item === 'string') }
        : fallback.sidebarView,
      backgroundOpen: typeof stored.backgroundOpen === 'boolean' ? stored.backgroundOpen : fallback.backgroundOpen,
      stepsDefaultExpanded: typeof stored.stepsDefaultExpanded === 'boolean' ? stored.stepsDefaultExpanded : fallback.stepsDefaultExpanded,
      viewportAnchors: stored.viewportAnchors && typeof stored.viewportAnchors === 'object' && !Array.isArray(stored.viewportAnchors)
        ? stored.viewportAnchors
        : fallback.viewportAnchors,
    }
  } catch {
    return fallback
  }
}

export function persistView(value: PersistedView): void {
  const { theme, messageFontSize, accentColor, selectedWorkspaceRoot, selectedSessionId, lastModelSelection, sidebarWidth, filePanelWidth, sidebarCollapsed, sidebarView, backgroundOpen, stepsDefaultExpanded, viewportAnchors } = value
  const view: PersistedView = { theme, messageFontSize, accentColor, selectedWorkspaceRoot, selectedSessionId, lastModelSelection, sidebarWidth, filePanelWidth, sidebarCollapsed, sidebarView, backgroundOpen, stepsDefaultExpanded, viewportAnchors }
  localStorage.setItem(VIEW_STORAGE_KEY, JSON.stringify(view))
}

export function clampFilePanelWidth(value: number): number {
  return Math.min(Math.max(320, innerWidth - 640), Math.max(320, value))
}

export function clampSidebarWidth(value: number): number {
  return Math.min(420, Math.max(220, value))
}
