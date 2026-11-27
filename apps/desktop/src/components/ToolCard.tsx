import { useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { CircleAlert, CircleX, LoaderCircle, FileText, FolderSearch, SquareTerminal, Image, ListTodo, MessageCircleQuestion, Search, Globe } from 'lucide-react'
import { appStore, useAppStore } from '../appStore'
import { factStatusText } from '../copy'
import { useSelectionGuard } from '../interactions'
import { formatTurnDuration, toolDisplay } from '../timeline'
import { workspaceRelativePath } from '../workspace'
import type { HistoryItem, ReadSource, ToolStatus, UserQuestion, UserQuestionAnswer, VisibleBackgroundTask } from '../protocol'
import { CommandOutput } from './CommandOutput'
import { Disclosure } from './Disclosure'
import { EditDiff } from './EditDiff'
import { ExpandChevron } from './ExpandChevron'
import { AttachedImages } from './Images'
import { parseTodos, TodoList, todoSummary } from './TodoList'

type ToolItem = Extract<HistoryItem, { type: 'tool' }>
type ArgRecord = Record<string, unknown>

interface ToolPresentation {
  title?: string
  fileArg?: boolean
  diffOutput?: boolean
  background?: boolean
  tracksLiveTasks?: boolean
  icon?: typeof FileText
  argument?: (input: ArgRecord) => string | null
  summary?: (input: ArgRecord) => string
}

const textField = (field: string) => (input: ArgRecord): string | null =>
  typeof input[field] === 'string' ? input[field] : null

const fileToolSpec = (extra?: Partial<ToolPresentation>): ToolPresentation => ({
  fileArg: true,
  argument: textField('path'),
  ...extra,
})

const toolPresentations: Record<string, ToolPresentation> = {
  bash: {
    background: true,
    tracksLiveTasks: true,
    icon: SquareTerminal,
    argument: textField('command'),
    summary: input => {
      const description = typeof input.description === 'string' ? input.description.trim() : ''
      return firstLine(description || (typeof input.command === 'string' ? input.command : ''))
    },
  },
  task_list: { background: true, summary: () => '列出后台命令' },
  task_output: { background: true, summary: () => '查看后台命令输出' },
  task_stop: { background: true, summary: () => '停止后台命令' },
  read: fileToolSpec(),
  grep: {
    icon: Search,
    argument: input => {
      const pattern = typeof input.pattern === 'string' ? input.pattern.trim() : ''
      const scope = [input.path, input.glob]
        .filter((value): value is string => typeof value === 'string' && value.trim() !== '')
        .map(value => value.trim())
        .join(' · ')
      if (pattern && scope) return `${pattern} (${scope})`
      return pattern || scope || null
    },
  },
  glob: {
    icon: FolderSearch,
    argument: input => {
      const pattern = typeof input.pattern === 'string' ? input.pattern.trim() : ''
      const path = typeof input.path === 'string' ? input.path.trim() : ''
      if (pattern && path) return `${pattern} (${path})`
      return pattern || path || null
    },
  },
  edit: fileToolSpec({ diffOutput: true }),
  write: fileToolSpec({ diffOutput: true }),
  view_image: fileToolSpec({ icon: Image }),
  web_search: { title: '联网搜索', icon: Search, argument: textField('query') },
  web_fetch: { title: '读取网页', icon: Globe, argument: textField('url') },
  skill: { title: '读取技能', argument: textField('name') },
  todo: {
    title: '待办',
    icon: ListTodo,
    summary: input => {
      const todos = parseTodos(input)
      return todos !== null ? todoSummary(todos) : ''
    },
  },
  ask_user_question: {
    title: '询问用户',
    icon: MessageCircleQuestion,
    summary: input => {
      const questions = input.questions
      const question = Array.isArray(questions) ? asRecord(questions[0]).question : undefined
      return typeof question === 'string' ? firstLine(question) : ''
    },
  },
  mcp_schema: {
    argument: input => {
      const server = typeof input.server === 'string' ? input.server.trim() : ''
      const tools = Array.isArray(input.tools)
        ? input.tools.filter((item): item is string => typeof item === 'string' && item.trim() !== '').map(item => item.trim())
        : []
      if (server && tools.length > 0) return `${server} / ${tools.join(', ')}`
      return tools.join(', ') || server || null
    },
  },
  mcp_call: {
    argument: input => {
      const server = typeof input.server === 'string' ? input.server.trim() : ''
      const tool = typeof input.tool === 'string' ? input.tool.trim() : ''
      if (server && tool) return `${server} / ${tool}`
      return tool || server || null
    },
  },
}

export function toolTracksLiveTasks(name: string): boolean {
  return toolPresentations[name]?.tracksLiveTasks === true
}

export function useBackgroundTasks(sessionId: string | undefined, enabled: boolean): VisibleBackgroundTask[] | undefined {
  const { liveSessions } = useAppStore(
    ['liveSessions'],
    enabled
      ? (previous, next) => previous.liveSessions[sessionId ?? '']?.backgroundTasks === next.liveSessions[sessionId ?? '']?.backgroundTasks
      : () => true,
  )
  return enabled ? liveSessions[sessionId ?? '']?.backgroundTasks : undefined
}

interface ToolCardProps {
  item: ToolItem
  itemKey: string
  cwd: string
  userHome?: string | null
  sessionId?: string
  onDisclosureChange: (element: HTMLElement) => void
}

export function ToolCard({ item, itemKey, cwd, userHome, sessionId, onDisclosureChange }: ToolCardProps) {
  const [expanded, setExpanded] = useState(false)
  const selectionGuard = useSelectionGuard(onDisclosureChange)
  const spec = toolPresentations[item.name]
  const tasks = useBackgroundTasks(sessionId, spec?.tracksLiveTasks === true)
  const { status, shown } = toolDisplay(item, tasks)
  const input = asRecord(item.args)
  const argument = spec?.argument?.(input) ?? null
  const filePath = spec?.fileArg && argument?.trim() ? argument : null
  const title = spec?.title ?? item.name
  const summary = filePath !== null
    ? displayPath(filePath, cwd, userHome)
    : (spec?.summary ? spec.summary(input) : firstLine(argument ?? '')) || firstLine(item.result?.output ?? '')

  return (
    <article className="timeline-item activity-step" data-item-id={itemKey} aria-label={`${title}，${factStatusText[status]}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <span className="step-label">
          <span className="step-icon" aria-hidden="true"><StepStatusIcon name={item.name} status={status} /></span>
          <span className="step-title">{title}</span>
        </span>
        <span className="step-summary">{summary}</span>
        {status === 'interrupted' && <span className="item-status">{factStatusText.interrupted}</span>}
        {status === 'pending' && <span className="item-status">{factStatusText.pending}</span>}
        <ExpandChevron expanded={expanded} />
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        {shown.result?.durationMs !== undefined && <div className="tool-duration">耗时 {formatTurnDuration(shown.result.durationMs)}</div>}
        <div className="timeline-body activity-output"><ToolOutput item={shown} spec={spec} filePath={filePath} command={item.name === 'bash' ? argument : null} cwd={cwd} />{sessionId && item.result?.images && <AttachedImages sessionId={sessionId} images={item.result.images} />}</div>
      </div></Disclosure>
    </article>
  )
}

function QuestionAnswers({ questions, output }: { questions?: UserQuestion[]; output: string }) {
  let parsed: { answers?: unknown }
  try {
    parsed = JSON.parse(output) as { answers?: unknown }
  } catch {
    return <pre><code>{output}</code></pre>
  }
  const answers = Array.isArray(parsed?.answers) ? parsed.answers as UserQuestionAnswer[] : null
  if (!answers) return <pre><code>{output}</code></pre>
  return <div>
    {answers.map((answer, index) => {
      const question = questions?.find(value => value.id === answer.id)
      const text = answer.text.trim()
      const reply = answer.skipped ? '跳过' : [...answer.selected, ...(text ? [text] : [])].join('、')
      return <div key={index} className="question-answer-result">
        <p className="question-result-title">{question?.question ?? answer.id}</p>
        <p>{reply}</p>
      </div>
    })}
  </div>
}

function ToolOutput({ item: fact, spec, filePath, command, cwd }: {
  item: ToolItem; spec: ToolPresentation | undefined; filePath: string | null; command: string | null; cwd: string
}) {
  const input = fact.args
  const background = spec?.background === true
  const displayJson = (key: string, value: unknown) => background && (key === 'taskId' || key === 'task_id') ? undefined : value
  const parameters = JSON.stringify(input, displayJson, 2)
  if (fact.name === 'ask_user_question' && fact.status === 'completed' && fact.result) {
    return <QuestionAnswers questions={(input as { questions?: UserQuestion[] })?.questions} output={fact.result.output} />
  }
  if (fact.name === 'todo' && fact.status !== 'failed') {
    const todos = parseTodos(input)
    if (todos !== null) return <TodoList todos={todos} />
  }
  let output = fact.result?.output ?? ''
  if (background && fact.name !== 'bash' && output !== '') {
    try { output = JSON.stringify(JSON.parse(output), displayJson, 2) }
    catch {}
  }
  if (command !== null) return <CommandOutput command={command} output={output} />
  if (spec?.diffOutput && filePath !== null && fact.status === 'completed') {
    const relPath = workspaceRelativePath(filePath, cwd)
    return <EditDiff path={filePath} output={output} onOpenFile={() => {
      if (relPath !== null) appStore.openChangedFiles(cwd, relPath)
      else appStore.openFile(filePath, cwd)
    }} />
  }
  const readSource = fact.result?.readSource
  if (filePath !== null && (output !== '' || (readSource !== undefined && readSource.lineCount > 0)) && fact.status !== 'failed') return <div>
    <div className="tool-output-header">
      {fact.name === 'read'
        ? <button type="button" className="tool-file-button" title={`在文件栏打开 ${filePath}`} onClick={() => appStore.openFile(filePath, cwd)}>{filePath}</button>
        : filePath}
    </div>
    {readSource
      ? <div className="tool-lines">{readOutputLines(output, readSource).map((line, index) => (
        <div key={index} className="tool-line">{line.number !== undefined && <span className="tool-line-number">{line.number}</span>}<span>{line.text}</span></div>
      ))}</div>
      : <pre><code>{output}</code></pre>}
  </div>
  return (
    <div>
      {parameters && parameters !== '{}' && <ToolSection label="参数"><pre><code>{parameters}</code></pre></ToolSection>}
      {output !== '' && (
        <ToolSection label={fact.status === 'failed' ? '错误' : '输出'}>
          <pre><code>{output}</code></pre>
        </ToolSection>
      )}
    </div>
  )
}

function ToolSection({ label, children }: { label: string; children: ReactNode }) {
  return <section className="timeline-section">
    <h4>{label}</h4>
    {children}
  </section>
}

export function StepStatusIcon({ name, status }: { name: string; status: ToolStatus }) {
  const prevStatus = useRef<ToolStatus>(status)
  const [justCompleted, setJustCompleted] = useState(false)
  useLayoutEffect(() => {
    const prev = prevStatus.current
    prevStatus.current = status
    if (status === 'completed' && (prev === 'running' || prev === 'pending')) setJustCompleted(true)
    else if (status !== 'completed') setJustCompleted(false)
  }, [status])

  if (status === 'running') return <LoaderCircle size={14} className="spinner step-spinner" aria-hidden="true" />
  if (status === 'completed') {
    return (
      <svg className={`step-tick${justCompleted ? ' is-fresh' : ''}`} width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
        <circle className="step-tick-ring" cx="8" cy="8" r="6.25" />
        <path className="step-tick-check" d="M5.1 8.2l2 2 3.8-4.2" />
      </svg>
    )
  }
  if (status === 'failed') return <CircleAlert size={14} className="bad" aria-hidden="true" />
  if (status === 'interrupted') return <CircleX size={14} className="bad" aria-hidden="true" />
  const Icon = toolPresentations[name]?.icon ?? FileText
  return <Icon size={14} aria-hidden="true" />
}

function displayPath(path: string, cwd: string, userHome?: string | null): string {
  const normalized = path.replace(/\\/g, '/')
  const root = cwd.replace(/\\/g, '/').replace(/\/$/, '')
  if (normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)) return normalized.slice(root.length + 1)
  const home = userHome?.replace(/\\/g, '/').replace(/\/$/, '')
  if (home && normalized.toLowerCase() === home.toLowerCase()) return '~'
  if (home && normalized.toLowerCase().startsWith(`${home.toLowerCase()}/`)) return `~${normalized.slice(home.length)}`
  return path
}

function firstLine(text: string): string {
  return text.split(/\r?\n/, 1)[0]?.trim() ?? ''
}

function asRecord(value: unknown): ArgRecord {
  return typeof value === 'object' && value !== null ? value as ArgRecord : {}
}

function readOutputLines(text: string, source: ReadSource): Array<{ text: string; number?: number }> {
  const lines = text.replace(/\r\n/g, '\n').split('\n')
  return lines.map((line, index) => index < source.lineCount
    ? { text: line, number: source.startLine + index }
    : { text: line })
}
