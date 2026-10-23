import { memo, useId, useState } from 'react'
import { Terminal } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, useAppStore, type ActionError, type AppState } from '../appStore'
import type { BackgroundTask, BackgroundTaskState } from '../protocol'
import { formatTurnDuration } from '../timeline'
import { useSelectionGuard } from '../interactions'
import { CommandOutput } from './CommandOutput'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

const fields = ['selectedSessionId', 'session', 'liveSessions', 'pendingActions', 'actionErrors', 'connection'] as const
type BackgroundState = Pick<AppState, typeof fields[number]>
const emptyTasks: BackgroundTask[] = []
function selectedTasks(state: BackgroundState): BackgroundTask[] {
  const id = state.selectedSessionId
  if (id === null) return emptyTasks
  return state.liveSessions[id]?.backgroundTasks ?? emptyTasks
}

export const BackgroundTasksButton = memo(function BackgroundTasksButton({ onOpen }: { onOpen: (button: HTMLButtonElement) => void }) {
  const state = useAppStore(fields, (previous, next) => previous.selectedSessionId === next.selectedSessionId
    && selectedTasks(previous) === selectedTasks(next))
  const tasks = selectedTasks(state)
  if (tasks.length === 0) return null
  const running = tasks.filter(task => task.state.status === 'running').length
  return <button type="button" className="header-button background-tasks-button" aria-controls="background-panel"
    aria-label={running ? `查看后台命令，${running} 个运行中` : '查看已结束的后台命令'}
    onClick={event => onOpen(event.currentTarget)}>
    <Terminal size={14} aria-hidden="true" /><span>后台</span>
    {running > 0 && <span className="background-running-count">{running}</span>}
  </button>
})

export const BackgroundTasks = memo(function BackgroundTasks() {
  const state = useAppStore(fields, (previous, next) => previous.selectedSessionId === next.selectedSessionId
    && selectedTasks(previous) === selectedTasks(next) && previous.pendingActions === next.pendingActions
    && previous.actionErrors === next.actionErrors && previous.connection === next.connection)
  const tasks = selectedTasks(state)
  const sessionId = state.selectedSessionId
  if (sessionId === null || tasks.length === 0) return <p className="detail-empty">当前任务没有后台命令。</p>
  const groups = [
    { label: '运行中', tasks: tasks.filter(task => task.state.status === 'running') },
    { label: '已结束', tasks: tasks.filter(task => task.state.status !== 'running') },
  ]
  return <section className="background-tasks" aria-label="后台命令">
    {/* 行保持同级和稳定身份，命令结束移入下一组时保留展开的输出。 */}
    {groups.flatMap(group => group.tasks.length === 0 ? [] : [
      <h2 key={group.label}>{group.label}<span>{group.tasks.length}</span></h2>,
      ...group.tasks.map(task => {
        const origin = actionOrigin.control(sessionId, task.taskId)
        return <BackgroundTaskRow key={`${sessionId}:${task.taskId}`} task={task} sessionId={sessionId}
          pending={state.pendingActions.has(pendingKey('session.stopBackgroundTask', origin))}
          error={state.actionErrors[origin]} connected={state.connection === 'ready'} />
      }),
    ])}
  </section>
})

const BackgroundTaskRow = memo(function BackgroundTaskRow({ task, sessionId, pending, error, connected }: {
  task: BackgroundTask; sessionId: string; pending: boolean; error: ActionError | undefined; connected: boolean
}) {
  const [expanded, setExpanded] = useState(false)
  const outputId = useId()
  const guard = useSelectionGuard()
  const running = task.state.status === 'running'
  const description = task.description || task.command
  const failed = task.state.status === 'failed' || task.state.status === 'interrupted'
    || (task.state.status === 'exited' && task.state.exitCode !== 0)
  return <article className={`background-task${failed ? ' is-failed' : ''}`}>
    <div className="background-task-row">
      <button type="button" className="activity-toggle" aria-expanded={expanded} aria-controls={outputId}
        title={description} {...guard(() => setExpanded(value => !value))}>
        <ExpandChevron expanded={expanded} size={14} />
        <span className="background-task-description">{description}</span>
      </button>
      <span className="background-task-status" role="status">{statusLabel(task.state)}</span>
      {running && <button type="button" className="quiet-button danger" disabled={pending || !connected}
        aria-busy={pending} aria-label={`${error ? '重试停止' : '停止'}后台命令：${description}`}
        onClick={() => void appStore.stopBackgroundTask(sessionId, task.taskId)}>
        {pending ? '正在停止…' : error ? '重试停止' : '停止命令'}
      </button>}
    </div>
    {error && <div className="background-task-error" role="alert">
      <span>{error.message}</span>{error.recovery && <span>{error.recovery}</span>}
    </div>}
    <div id={outputId}><Disclosure open={expanded}><div className="background-task-details">
      <div className="tool-duration">{task.taskId}{'durationMs' in task.state && <> · 耗时 {formatTurnDuration(task.state.durationMs)}</>}</div>
      {task.state.status === 'failed' && <p className="background-task-error">{task.state.message}</p>}
      <div className="timeline-body activity-output" tabIndex={0} aria-label={`${description}，命令与输出`}>
        <CommandOutput command={task.command} output={task.output} />
        {task.output === '' && <p className="background-task-empty">尚无输出</p>}
      </div>
    </div></Disclosure></div>
  </article>
})

function statusLabel(state: BackgroundTaskState): string {
  switch (state.status) {
    case 'running': return '运行中'
    case 'exited': return `已退出 · 退出码 ${state.exitCode}`
    case 'stopped': return '已停止'
    case 'failed': return '执行失败'
    case 'interrupted': return '重启后已中断'
  }
}
