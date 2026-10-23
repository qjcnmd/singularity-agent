import { workspaceKey } from '../workspace'
import { Plus } from 'lucide-react'
import { useRef, useState } from 'react'
import { navigateList, useSelectionGuard, useDismissOnOutside } from '../interactions'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import { PickerSurface } from './PickerSurface'
import { ExpandChevron } from './ExpandChevron'
import { WorkspaceIcon } from './WorkspaceAppearancePicker'
import { workspaceName } from '../copy'

/** 新建任务页顶部的项目选择器状态。 */
type Props = Pick<AppState, 'bootstrap' | 'selectedWorkspaceRoot' | 'workspaceAppearance' | 'pendingActions'>

export function WorkspacePicker({ state }: { state: Props }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const anchor = useRef<HTMLButtonElement>(null)
  const guard = useSelectionGuard()
  const workspaces = state.bootstrap?.workspaces ?? []
  const selected = workspaces.find(workspace => workspaceKey(workspace.root) === state.selectedWorkspaceRoot)
  useDismissOnOutside(root, open, () => setOpen(false))
  const pick = (id: string) => {
    setOpen(false)
    anchor.current?.focus()
    if (id === 'add') appStore.openDirectoryPicker()
    else void appStore.newTask(id)
  }
  return <div ref={root} className="hero-workspace" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false)
  }} onKeyDown={event => {
    if (event.key === 'Escape' && open) {
      event.preventDefault()
      event.stopPropagation()
      setOpen(false)
      anchor.current?.focus()
    } else if (open && navigateList(event.key, [...event.currentTarget.querySelectorAll<HTMLButtonElement>('.workspace-picker-options button')])) event.preventDefault()
  }}>
    <button ref={anchor} type="button" className="workspace-picker-trigger" disabled={state.pendingActions.has(pendingKey('directory.pick', actionOrigin.directoryPicker))} aria-label="选择项目" aria-controls="workspace-picker-options" aria-expanded={open} onClick={() => setOpen(value => !value)}>
      <WorkspaceIcon appearance={selected ? state.workspaceAppearance[workspaceKey(selected.root)] : undefined} /><span>{selected ? workspaceName(selected) : '选择项目'}</span><ExpandChevron expanded={open} size={12} />
    </button>
    <PickerSurface open={open}><div className="workspace-picker-options" id="workspace-picker-options" role="group" aria-label="项目选项">
      {workspaces.map(workspace => <button key={workspaceKey(workspace.root)} type="button" aria-pressed={workspaceKey(workspace.root) === state.selectedWorkspaceRoot} {...guard(() => pick(workspaceKey(workspace.root)))}>
        <WorkspaceIcon appearance={state.workspaceAppearance[workspaceKey(workspace.root)]} /><span>{workspaceName(workspace)}</span><span aria-hidden="true">{workspaceKey(workspace.root) === state.selectedWorkspaceRoot ? '✓' : ''}</span>
      </button>)}
      <button type="button" {...guard(() => pick('add'))}><Plus size={18} strokeWidth={1.7} aria-hidden="true" /><span>添加项目</span></button>
    </div></PickerSurface>
  </div>
}
