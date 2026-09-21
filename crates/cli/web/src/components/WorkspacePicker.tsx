import { Plus } from 'lucide-react'
import { useRef, useState } from 'react'
import { navigateList, useSelectionGuard, useDismissOnOutside } from '../interactions'
import { appStore, pendingKey, type AppState } from '../appStore'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import { WorkspaceIcon } from './WorkspaceAppearancePicker'

/** 只声明本组件读取的字段：父级按同一份清单订阅。 */
type Props = Pick<AppState, 'bootstrap' | 'selectedWorkspaceId' | 'workspaceAppearance' | 'pendingActions'>

export function WorkspacePicker({ state }: { state: Props }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const anchor = useRef<HTMLButtonElement>(null)
  const guard = useSelectionGuard()
  const workspaces = state.bootstrap?.workspaces ?? []
  const selected = workspaces.find(workspace => workspace.workspaceId === state.selectedWorkspaceId)
  useDismissOnOutside(root, open, () => setOpen(false))
  const pick = (id: string) => {
    setOpen(false)
    anchor.current?.focus()
    if (id === 'add') appStore.openDirectoryPicker()
    else void appStore.createSession(id, true)
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
    <button ref={anchor} type="button" className="workspace-picker-trigger" disabled={state.pendingActions.has(pendingKey('directory.pick', 'directory:picker'))} aria-label="选择项目" aria-controls="workspace-picker-options" aria-expanded={open} onClick={() => setOpen(value => !value)}>
      <WorkspaceIcon appearance={selected ? state.workspaceAppearance[selected.workspaceId] : undefined} /><span>{selected?.name ?? '选择项目'}</span><ExpandChevron expanded={open} size={12} />
    </button>
    <Disclosure className="picker-disclosure" open={open}><div className="workspace-picker-options" id="workspace-picker-options" role="group" aria-label="项目选项">
      {workspaces.map(workspace => <button key={workspace.workspaceId} type="button" aria-pressed={workspace.workspaceId === state.selectedWorkspaceId} {...guard(() => pick(workspace.workspaceId))}>
        <WorkspaceIcon appearance={state.workspaceAppearance[workspace.workspaceId]} /><span>{workspace.name}</span><span aria-hidden="true">{workspace.workspaceId === state.selectedWorkspaceId ? '✓' : ''}</span>
      </button>)}
      <button type="button" {...guard(() => pick('add'))}><Plus size={18} strokeWidth={1.7} aria-hidden="true" /><span>添加项目</span></button>
    </div></Disclosure>
  </div>
}
