import { LoaderCircle, Square } from 'lucide-react'
import { memo, useId, useState } from 'react'
import { actionOrigin, appStore, pendingKey, useAppStore, type ActionError, type AppState } from '../appStore'
import type { BackgroundTask } from '../protocol'
import { useSelectionGuard } from '../interactions'
import { CommandOutput } from './CommandOutput'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

const fields = ['selectedSessionId', 'liveSessions', 'pendingActions', 'actionErrors', 'connection'] as const
type BackgroundState = Pick<AppState, typeof fields[number]>
const emptyTasks: BackgroundTask[] = []
function selectedTasks(state: BackgroundState): BackgroundTask[] {
  const id = state.selectedSessionId
  if (id === null) return emptyTasks
  return state.liveSessions[id]?.backgroundTasks ?? emptyTasks
}

export const BackgroundTasks = memo(function BackgroundTasks() {
  const state = useAppStore(fields, (previous, next) => previous.selectedSessionId === next.selectedSessionId
    && selectedTasks(previous) === selectedTasks(next) && previous.pendingActions === next.pendingActions
    && previous.actionErrors === next.actionErrors && previous.connection === next.connection)
  const tasks = selectedTasks(state).filter(task => task.state.status === 'running')
  const sessionId = state.selectedSessionId
  if (sessionId === null || tasks.length === 0) return null
  return <section className="background-tasks" aria-label="后台命令">
    {tasks.map(task => {
      const origin = actionOrigin.control(sessionId, task.taskId)
      return <BackgroundTaskRow key={`${sessionId}:${task.taskId}`} task={task} sessionId={sessionId}
        pending={state.pendingActions.has(pendingKey('session.stopBackgroundTask', origin))}
        error={state.actionErrors[origin]} connected={state.connection === 'ready'} />
    })}
  </section>
})

const BackgroundTaskRow = memo(function BackgroundTaskRow({ task, sessionId, pending, error, connected }: {
  task: BackgroundTask; sessionId: string; pending: boolean; error: ActionError | undefined; connected: boolean
}) {
  const [expanded, setExpanded] = useState(false)
  const outputId = useId()
  const guard = useSelectionGuard()
  const description = task.description || task.command
  return <article className="task-card">
    <div className="task-row">
      <button type="button" className="expandable-toggle" aria-expanded={expanded} aria-controls={outputId}
        {...guard(() => setExpanded(value => !value))}>
        <ExpandChevron expanded={expanded} size={14} />
        <span className="task-name">{description}</span>
      </button>
      <span className="task-status" role="status"><i />运行中</span>
      <button type="button" className="task-stop" disabled={pending || !connected}
        aria-busy={pending} aria-label={`${error ? '重试停止' : '停止'}后台命令：${description}`}
        onClick={() => void appStore.stopBackgroundTask(sessionId, task.taskId)}>
        {pending ? <LoaderCircle size={13} className="spinner" aria-hidden="true" /> : <Square size={9} fill="currentColor" strokeWidth={0} />}
      </button>
    </div>
    {error && <div className="background-task-error" role="alert">
      <span>{error.message}</span>
    </div>}
    <div id={outputId}><Disclosure open={expanded}><div className="task-details">
      <div className="timeline-body activity-output" tabIndex={0} aria-label={`${description}，命令与输出`}>
        <CommandOutput command={task.command} output={task.output} />
      </div>
    </div></Disclosure></div>
  </article>
})
