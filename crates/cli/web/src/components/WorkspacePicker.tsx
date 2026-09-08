import { useRef, useState } from 'react'
import { workbenchStore, type WorkbenchState } from '../store'
import { Menu } from './Menu'
import { WorkspaceIcon } from './WorkspaceAppearancePicker'

export function WorkspacePicker({ state }: { state: WorkbenchState }) {
  const [open, setOpen] = useState(false)
  const anchor = useRef<HTMLButtonElement>(null)
  const workspaces = state.bootstrap?.workspaces ?? []
  const selected = workspaces.find(workspace => workspace.workspaceId === state.selectedWorkspaceId)
  return <div className="hero-workspace">
    <button ref={anchor} type="button" className="workspace-picker-trigger" disabled={workbenchStore.isPending('directory.pick', 'directory:picker')} aria-label="选择项目" aria-haspopup="menu" aria-expanded={open} onClick={() => {
      setOpen(value => !value)
    }}><WorkspaceIcon appearance={selected ? state.workspaceAppearance[selected.workspaceId] : undefined} /><span>{selected?.name ?? '选择项目'}</span><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg></button>
    {open && <Menu anchor={anchor} label="选择项目" onClose={() => setOpen(false)} entries={[
      ...workspaces.map(workspace => ({id: workspace.workspaceId, label: workspace.name, checked: workspace.workspaceId === state.selectedWorkspaceId})),
      { id: 'add', label: '＋ 添加项目', divider: true },
    ]} onPick={id => { if (id === 'add') workbenchStore.openDirectoryPicker(); else void workbenchStore.createSession(id, true) }} />}
  </div>
}
