import { useId, useLayoutEffect, useRef, useState } from 'react'
import { actionOrigin, appStore, pendingKey, useAppStore, type AppState } from '../appStore'
import { formatTokenCount, workspaceName } from '../copy'
import { navigateList, useDismissOnOutside } from '../interactions'
import type { ContextGroup, ContextModule } from '../protocol'
import { workspaceKey } from '../workspace'
import { Plus } from 'lucide-react'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

type ModuleQuery =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ready'; modules: ContextModule[] }
  | { status: 'error'; error: Error }

/** 新任务页读取当前项目的上下文胶囊；切换项目作废上一次读取，失败可重试。 */
function useContextModules(workspaceRoot: string | null, connection: string) {
  const [query, setQuery] = useState<ModuleQuery>({ status: 'idle' })
  const [reload, setReload] = useState(0)
  useLayoutEffect(() => {
    if (workspaceRoot === null || connection !== 'ready') {
      setQuery({ status: 'idle' })
      return
    }
    setQuery({ status: 'loading' })
    let active = true
    void appStore.transport.rpc('context.preview', { workspaceRoot }).then(
      modules => { if (active) setQuery({ status: 'ready', modules }) },
      error => { if (active) setQuery({ status: 'error', error: error instanceof Error ? error : new Error(String(error)) }) },
    )
    return () => { active = false }
  }, [workspaceRoot, connection, reload])
  return { query, retry: () => setReload(value => value + 1) }
}

const groupTitles: Record<ContextGroup, string> = { instructions: '指令', tools: '工具', skills: '技能', mcp: 'MCP' }
const groupOrder: ContextGroup[] = ['instructions', 'tools', 'skills', 'mcp']

/** 新任务页输入框上方：项目选择和上下文胶囊。胶囊决定接下来这个任务注入哪些内容，创建后固定。 */
export function ContextCapsules() {
  const state = useAppStore(['drafts', 'bootstrap', 'connection', 'selectedWorkspaceRoot', 'pendingActions'])
  const { query, retry } = useContextModules(state.selectedWorkspaceRoot, state.connection)
  const disabled = new Set(appStore.inputDraft().disabledModules)
  return <div className="context-capsules">
    <ProjectCapsule state={state} />
    <div className="context-module-list">
      {query.status === 'loading' && <p className="capsule-status" role="status">正在读取上下文…</p>}
      {query.status === 'error' && <p className="capsule-status form-error" role="alert">{query.error.message}
        <button type="button" className="quiet-button" onClick={retry}>重试</button></p>}
      {query.status === 'ready' && <ModuleGroups modules={query.modules} disabled={disabled} />}
    </div>
  </div>
}

function ModuleGroups({ modules, disabled }: { modules: ContextModule[]; disabled: ReadonlySet<string> }) {
  const total = modules.filter(module => module.locked || !disabled.has(module.id)).reduce((sum, module) => sum + module.tokens, 0)
  return <>
    {groupOrder.map(group => {
      const members = modules.filter(module => module.group === group)
      return members.length > 0 && <section className="capsule-group" key={group} aria-label={groupTitles[group]}>
        <h4>{groupTitles[group]}</h4>
        <div className="capsules">{members.map(module => <Capsule key={module.id} module={module} on={module.locked || !disabled.has(module.id)} />)}</div>
      </section>
    })}
    <p className="capsule-total" title="本次任务注入的指令和工具定义的估算 Token 数">∼ {formatTokenCount(total).toLowerCase()} tokens</p>
  </>
}

/** 胶囊里的圆圈勾和估算数字；运行状态面板用同一套外观显示已加载的模块。 */
export function CapsuleIcon() {
  return <svg className="capsule-icon" viewBox="0 0 16 16" aria-hidden="true"><circle className="dot" cx="8" cy="8" r="2" /><circle className="ring" cx="8" cy="8" r="6.4" /><path className="tick" d="M5 8.3l2 2 3.7-4" /></svg>
}

export function CapsuleTokens({ tokens }: { tokens: number }) {
  return tokens > 0 ? <span className="capsule-tokens">∼ {formatTokenCount(tokens).toLowerCase()}</span> : null
}

function Capsule({ module, on }: { module: ContextModule; on: boolean }) {
  return <button type="button" role="switch" aria-checked={on} aria-disabled={module.locked} title={module.locked ? `${module.detail}（基础能力，不能关闭）` : module.detail}
    className={`capsule${on ? ' is-on' : ''}`} onClick={() => { if (!module.locked) appStore.setModuleEnabled(module.id, !on) }}>
    <CapsuleIcon />
    <span className="capsule-name">{module.label}</span>
    <CapsuleTokens tokens={module.tokens} />
  </button>
}

type ProjectState = Pick<AppState, 'bootstrap' | 'selectedWorkspaceRoot' | 'pendingActions'>

/** 项目选择：沿用消息块展开动画，菜单由 CSS 在窗口内定位。 */
function ProjectCapsule({ state }: { state: ProjectState }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const anchor = useRef<HTMLButtonElement>(null)
  const menuId = useId()
  const workspaces = state.bootstrap?.workspaces ?? []
  const selected = workspaces.find(workspace => workspaceKey(workspace.root) === state.selectedWorkspaceRoot)
  useDismissOnOutside(root, open, () => setOpen(false))
  const close = () => { setOpen(false); anchor.current?.focus() }
  const pick = (id: string) => {
    close()
    if (id === 'add') appStore.openDirectoryPicker()
    else if (id !== state.selectedWorkspaceRoot) appStore.newTask(id)
  }
  return <div ref={root} className="project-capsule" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false)
  }} onKeyDown={event => {
    if (event.key === 'Escape' && open) {
      event.preventDefault()
      event.stopPropagation()
      close()
    } else if (open && navigateList(event.key, [...event.currentTarget.querySelectorAll<HTMLButtonElement>('.project-menu button')])) event.preventDefault()
  }}>
    <button ref={anchor} type="button" className="capsule project-trigger" aria-label="选择项目" aria-controls={menuId} aria-expanded={open}
      disabled={state.pendingActions.has(pendingKey('directory.pick', actionOrigin.directoryPicker))} onClick={() => setOpen(value => !value)}>
      <span className="capsule-name">{selected ? workspaceName(selected) : '选择项目'}</span><ExpandChevron expanded={open} size={12} />
    </button>
    <div className="project-menu" data-open={open}>
      <Disclosure open={open}><div id={menuId} className="project-options" role="group" aria-label="项目选项">
        {workspaces.map(workspace => <button key={workspaceKey(workspace.root)} type="button" aria-pressed={workspaceKey(workspace.root) === state.selectedWorkspaceRoot} onClick={() => pick(workspaceKey(workspace.root))}>
          <span>{workspaceName(workspace)}</span><span className="check" aria-hidden="true">{workspaceKey(workspace.root) === state.selectedWorkspaceRoot ? '✓' : ''}</span>
        </button>)}
        <button type="button" onClick={() => pick('add')}><Plus size={14} strokeWidth={1.7} aria-hidden="true" /><span>添加项目</span></button>
      </div></Disclosure>
    </div>
  </div>
}
