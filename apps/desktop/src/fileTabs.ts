import type { ChangedFile } from './protocol'
import { pathBaseName, resolveFileUrl, workspaceKey } from './workspace'

export interface FileTab {
  id: string; path: string; cwd: string; title: string
  mode: 'file' | 'changes'
  focusPath?: string
  initialChanges?: ChangedFile[]
  opened: number
}

export interface FileTabState {
  fileTabs: readonly FileTab[]
  activeFileTab: string | null
}

function upsertFileTab(state: FileTabState, tab: FileTab): FileTabState {
  const fileTabs = state.fileTabs.some(item => item.id === tab.id)
    ? state.fileTabs.map(item => item.id === tab.id
      ? { ...item, focusPath: tab.focusPath, initialChanges: tab.initialChanges ?? item.initialChanges, opened: item.opened + 1 }
      : item)
    : [...state.fileTabs, tab]
  return { fileTabs, activeFileTab: tab.id }
}

export function openFileTab(
  state: FileTabState,
  path: string,
  cwd: string,
  userHome?: string | null,
  title = pathBaseName(path),
): FileTabState | null {
  const resolved = resolveFileUrl(path, cwd, userHome)
  if (!resolved) return null
  return upsertFileTab(state, { id: resolved.href.toLowerCase(), path, cwd, title, mode: 'file', opened: 0 })
}

export function openChangedFilesTab(
  state: FileTabState,
  cwd: string,
  focusPath?: string,
  initialChanges?: ChangedFile[],
): FileTabState {
  const id = `changes:${workspaceKey(cwd)}`
  return upsertFileTab(state, { id, path: cwd, cwd, title: 'Changes', mode: 'changes', focusPath, initialChanges, opened: 0 })
}

export function filterFileTabs(
  state: FileTabState,
  keep: (tab: FileTab) => boolean,
): FileTabState | null {
  const fileTabs = state.fileTabs.filter(keep)
  if (fileTabs.length === state.fileTabs.length) return null
  if (state.activeFileTab === null || fileTabs.some(tab => tab.id === state.activeFileTab)) {
    return { fileTabs, activeFileTab: state.activeFileTab }
  }
  const previousIndex = state.fileTabs.findIndex(tab => tab.id === state.activeFileTab)
  const fallback = state.fileTabs.slice(previousIndex + 1).find(keep)
    ?? state.fileTabs.slice(0, previousIndex).findLast(keep)
  return { fileTabs, activeFileTab: fallback?.id ?? null }
}

export function retainFileTabs(
  state: FileTabState,
  existingWorkspaces: ReadonlySet<string>,
): FileTabState | null {
  if (!state.fileTabs.some(tab => tab.mode === 'changes')) return null
  return filterFileTabs(state, tab => tab.mode !== 'changes' || existingWorkspaces.has(workspaceKey(tab.cwd)))
}

export function closeWorkspaceTabs(state: FileTabState, workspaceRoot: string): FileTabState | null {
  const wsKey = workspaceKey(workspaceRoot)
  return filterFileTabs(state, tab => tab.mode !== 'changes' || workspaceKey(tab.cwd) !== wsKey)
}

export function closeFileTab(state: FileTabState, id: string): FileTabState | null {
  return filterFileTabs(state, tab => tab.id !== id)
}

export function selectFileTab(state: FileTabState, id: string): FileTabState {
  return { fileTabs: state.fileTabs, activeFileTab: id }
}
