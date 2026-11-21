import { ExpandChevron } from './ExpandChevron'
import { APP_NAME } from '../identity'
import { AttachedImages, FileChips } from './Images'
import { QuestionPanel } from './QuestionPanel'
import { Disclosure } from './Disclosure'
import { ToolGroupRow } from './ToolGroupRow'
import { StepStatusIcon, ToolCard, toolTracksLiveTasks, useBackgroundTasks } from './ToolCard'
import { memo, useEffect, useState } from 'react'
import { CircleAlert, CircleX, LoaderCircle } from 'lucide-react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { pop } from '../motion'
import { useBodyCollapse, useSelectionGuard } from '../interactions'
import { MarkdownBody } from '../markdown'
import { contextRotationText, factStatusText, formatTokenCount } from '../copy'
import { formatClock, contextRotationResultText, formatTurnDuration, toolDisplay, type ActivityGroup, type ActivityRowItem, type TimelineContent } from '../timeline'
import type { HistoryItem, SessionModelUsage, ToolStatus, VisibleBackgroundTask } from '../protocol'

const previewLineCount = 8

interface Props {
  item: TimelineContent
  itemKey: string
  cwd: string
  userHome?: string | null
  sessionId?: string
  onDisclosureChange: (element: HTMLElement) => void
}

// 历史项复用稳定引用跳过重绘；流式更新只重新渲染当前活动项。
export const TimelineItem = memo(function TimelineItem({ item, itemKey, cwd, userHome, sessionId, onDisclosureChange }: Props) {
  const userLines = item.type === 'message' && item.role === 'user' ? item.text.trimEnd().split('\n') : []
  const hiddenLines = Math.max(0, userLines.length - previewLineCount)
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(false)
  const selectionGuard = useSelectionGuard(onDisclosureChange)
  const reduced = useReducedMotion()

  if (item.type === 'activity_group') {
    return <ActivityCard group={item} itemKey={itemKey} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />
  }
  if (item.type === 'tool_group') return <ToolGroupRow group={item} itemKey={itemKey} sessionId={sessionId} onDisclosureChange={onDisclosureChange}>
    {item.rows.map(row => <ToolCard key={row.key} item={row.item} itemKey={`${row.key}:group-child`} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />)}
  </ToolGroupRow>
  if (item.type === 'duration') return <TurnDuration itemKey={itemKey} {...item} />
  if (item.type === 'stats') return <MessageStatsLine itemKey={itemKey} stats={item.stats} at={item.at} />
  if (item.type === 'question') return (
    <motion.section
      initial={reduced ? false : { opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={reduced ? { duration: 0 } : { duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
      className="timeline-item activity-card question-card"
      data-item-id={itemKey}
      aria-label="询问用户"
    >
      <QuestionPanel
        key={item.request.itemId}
        request={item.request}
        headerMeta={item.duration && <TurnDuration inline {...item.duration} />}
      />
    </motion.section>
  )
  if (item.type === 'typing') return <article
    className="timeline-item message-item timeline-assistant" data-item-id={itemKey} aria-label="正在输入">
    <div className="message-bubble typing-bubble"><LoaderCircle size={13} className="spinner" aria-hidden="true" /></div>
  </article>

  if (item.type === 'terminal') {
    return <div className={`timeline-item message-item timeline-assistant${item.status === 'interrupted' ? ' terminal-interruption' : ''}`} data-item-id={itemKey}>
      {item.status === 'failed'
        ? <div className="message-bubble error-bubble" role="alert"><CircleAlert size={15} className="error-icon" aria-hidden="true" /><span>{item.text}</span></div>
        : <article className="activity-step"><div className="activity-toggle"><CircleX size={13} className="bad" /><span className="step-title">{item.text}</span></div></article>}
      {item.at && <time className="turn-stats" dateTime={item.at} title="结束时间">{formatClock(item.at, true)}</time>}
    </div>
  }

  if (item.type === 'message') {
    const body = canCollapse && !expanded ? userLines.slice(0, previewLineCount).join('\n') : item.text
    const attachments = <>
      {sessionId && item.images && <AttachedImages sessionId={sessionId} images={item.images} />}
      {item.files && <FileChips files={item.files} />}
    </>
    return (
      <motion.article initial={{ opacity: 0, y: item.role === 'user' ? 36 : 10, scale: item.role === 'user' ? .84 : .9 }} animate={{ opacity: 1, y: 0, scale: 1 }} transition={reduced ? { duration: 0 } : { ...pop, opacity: { duration: .16 } }} style={{ originX: item.role === 'user' ? 1 : 0, originY: 1 }}
        className={`timeline-item message-item timeline-${item.role} status-${item.status}`}
        data-item-id={itemKey}
        aria-label={`${item.role === 'user' ? '你' : APP_NAME}，${factStatusText[item.status]}`}
      >
        {item.role === 'user' && item.at && <time className="msg-time" dateTime={item.at}>{formatClock(item.at, true)}</time>}
        <div className={`message-bubble${item.status === 'failed' ? ' error-bubble' : ''}`}>
        {item.status === 'failed' && <CircleAlert size={15} className="error-icon" aria-hidden="true" />}
        {item.role === 'user' && attachments}
        <div className="timeline-body message-body">{item.role === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} streaming={item.status === 'running'} />}</div>
        {item.role === 'assistant' && attachments}
        {canCollapse && (
          <button type="button" className="expand-button" aria-expanded={expanded} {...selectionGuard(() => setExpanded((value) => !value))}>
            {expanded ? '收起' : `展开全文 · 还有 ${hiddenLines} 行`}
          </button>
        )}
        </div>
        {item.role === 'assistant' && <MessageStatsLine stats={item.stats} at={item.at} />}
      </motion.article>
    )
  }

  if (item.type === 'thinking') return <ReasoningRow item={item} itemKey={itemKey} onDisclosureChange={onDisclosureChange} />
  if (item.type === 'context_rotation' || item.type === 'context_rotation_result') return <ContextRotationRow item={item} itemKey={itemKey} onDisclosureChange={onDisclosureChange} />

  return <ToolCard item={item} itemKey={itemKey} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />
})

function summarizeActivity(rows: ActivityGroup['rows'], tasks: VisibleBackgroundTask[] | undefined) {
  let stepCount = 0
  let runningKind: ActivityRowItem['type'] | null = null
  let hasTyping = false
  let hasFailed = false
  let hasInterrupted = false
  let hasCompleted = false
  let terminalText: string | null = null
  let terminalFailed = false
  for (const { item } of rows) {
    if (item.type === 'typing') {
      hasTyping = true
      continue
    }
    if (item.type === 'terminal') {
      if (item.status === 'failed') terminalFailed = true
      else { hasInterrupted = true; terminalText = item.text }
      continue
    }
    if (item.type === 'tool_group') {
      stepCount += item.rows.length
      for (const sub of item.rows) {
        const status = toolDisplay(sub.item, tasks).status
        if (status === 'running') runningKind = 'tool_group'
        else if (status === 'failed') hasFailed = true
        else if (status === 'interrupted') hasInterrupted = true
        else if (status === 'completed') hasCompleted = true
      }
      continue
    }
    stepCount += 1
    const status: ToolStatus = item.type === 'tool'
      ? toolDisplay(item, tasks).status
      : item.type === 'context_rotation'
        ? 'completed'
        : item.status
    if (status === 'running') runningKind = item.type
    else if (status === 'failed') hasFailed = true
    else if (status === 'interrupted') hasInterrupted = true
    else if (status === 'completed') hasCompleted = true
  }
  const status: ToolStatus = runningKind !== null || hasTyping ? 'running'
    : terminalFailed ? 'failed'
    : terminalText !== null ? 'interrupted'
    : hasFailed ? 'failed'
    : hasInterrupted ? 'interrupted'
    : hasCompleted ? 'completed'
    : 'pending'
  let label: string
  if (runningKind !== null) {
    if (stepCount === 1 && runningKind === 'thinking') label = '思考中'
    else if (stepCount === 1 && runningKind === 'context_rotation_result') label = contextRotationText.running
    else if (runningKind === 'thinking') label = `思考中 · 共 ${stepCount} 步`
    else label = `执行中 · 共 ${stepCount} 步`
  } else if (hasTyping) {
    label = stepCount > 0 ? `等待回复 · 共 ${stepCount} 步` : '等待模型回复…'
  } else if (status === 'failed') {
    label = stepCount > 0 ? `执行失败 · ${stepCount} 个步骤` : '执行失败'
  } else if (status === 'interrupted') {
    const title = terminalText ?? '已中断'
    label = stepCount > 0 ? `${title} · ${stepCount} 个步骤` : title
  } else if (status === 'completed') {
    label = stepCount === 1 && rows[0]?.item.type === 'thinking' ? '思考过程' : `已完成 ${stepCount} 个步骤`
  } else {
    label = `等待执行 · ${stepCount} 个步骤`
  }
  return { status, label, stepCount }
}

function ActivityCard({ group, itemKey, cwd, userHome, sessionId, onDisclosureChange }: Omit<Props, 'item'> & { group: ActivityGroup }) {
  const reduced = useReducedMotion()
  const [collapsed, setCollapsed] = useState(false)
  const guard = useSelectionGuard(onDisclosureChange)
  const tracksLiveTasks = group.rows.some(({ item }) =>
    (item.type === 'tool' || item.type === 'tool_group') && toolTracksLiveTasks(item.name),
  )
  const tasks = useBackgroundTasks(sessionId, tracksLiveTasks)
  const { status, label, stepCount } = summarizeActivity(group.rows, tasks)
  const soloRow = group.rows.length === 1 ? group.rows[0].item : null
  const headerOnly = soloRow !== null && (soloRow.type === 'typing' || (soloRow.type === 'terminal' && soloRow.status === 'interrupted'))
  const collapsible = stepCount > 0
  const meta = group.duration
    ? <TurnDuration inline {...group.duration} />
    : soloRow?.type === 'terminal' && soloRow.at
      ? <time className="activity-card-meta" dateTime={soloRow.at} title="结束时间">{formatClock(soloRow.at, true)}</time>
      : null

  return (
    <motion.section
      initial={reduced ? false : { opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={reduced ? { duration: 0 } : { duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
      className={`timeline-item activity-card status-${status}`}
      data-item-id={itemKey}
      aria-label={label}
    >
      {collapsible ? (
        <button type="button" className="activity-card-header" aria-expanded={!collapsed} {...guard(() => setCollapsed(value => !value))}>
          <StepStatusIcon name="activity" status={status} />
          <span className={`activity-card-title${status === 'running' ? ' activity-shimmer' : ''}`}>{label}</span>
          {meta}
          <ExpandChevron expanded={!collapsed} size={14} />
        </button>
      ) : (
        <div className={`activity-card-header is-static${headerOnly ? ' is-header-only' : ''}`}>
          <StepStatusIcon name="activity" status={status} />
          <span className={`activity-card-title${status === 'running' ? ' activity-shimmer' : ''}`}>{label}</span>
          {meta}
        </div>
      )}
      {!headerOnly && (
        <div className={`content-disclosure${!collapsed || !collapsible ? ' is-open' : ''}`} inert={collapsible && collapsed} aria-hidden={collapsible && collapsed}>
          <div className="activity-card-body">
            <AnimatePresence initial={false}>
              {group.rows.map(row => (
                <motion.div
                  key={row.key}
                  className="activity-card-row"
                  initial={reduced ? false : { opacity: 0, height: 0 }}
                  animate={{ opacity: 1, height: 'auto' }}
                  exit={reduced ? undefined : { opacity: 0, height: 0 }}
                  transition={reduced ? { duration: 0 } : {
                    height: { duration: 0.24, ease: [0.22, 1, 0.36, 1] },
                    opacity: { duration: 0.18, ease: 'easeOut', delay: 0.04 },
                  }}
                >
                  <ActivityStepRow item={row.item} itemKey={row.key} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />
                </motion.div>
              ))}
            </AnimatePresence>
          </div>
        </div>
      )}
    </motion.section>
  )
}

const ActivityStepRow = memo(function ActivityStepRow({ item, itemKey, cwd, userHome, sessionId, onDisclosureChange }: Omit<Props, 'item'> & { item: ActivityRowItem }) {
  if (item.type === 'typing') {
    return (
      <div className="activity-status-row" data-item-id={itemKey} aria-label="正在等待模型回复">
        <LoaderCircle size={14} className="spinner step-spinner" aria-hidden="true" />
        <span className="step-title activity-shimmer">等待模型回复…</span>
      </div>
    )
  }
  if (item.type === 'terminal') {
    if (item.status === 'failed') {
      return (
        <div className="activity-status-row is-error" data-item-id={itemKey} role="alert">
          <CircleAlert size={14} className="bad" aria-hidden="true" />
          <span className="activity-error-text">{item.text}</span>
          {item.at && <time className="activity-row-time" dateTime={item.at} title="结束时间">{formatClock(item.at, true)}</time>}
        </div>
      )
    }
    return (
      <div className="activity-status-row is-interrupted" data-item-id={itemKey}>
        <CircleX size={14} className="bad" aria-hidden="true" />
        <span className="step-title">{item.text}</span>
        {item.at && <time className="activity-row-time" dateTime={item.at} title="结束时间">{formatClock(item.at, true)}</time>}
      </div>
    )
  }
  if (item.type === 'tool_group') {
    return <ToolGroupRow group={item} itemKey={itemKey} sessionId={sessionId} onDisclosureChange={onDisclosureChange}>
      {item.rows.map(row => <ToolCard key={row.key} item={row.item} itemKey={`${row.key}:group-child`} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />)}
    </ToolGroupRow>
  }
  if (item.type === 'thinking') return <ReasoningRow item={item} itemKey={itemKey} onDisclosureChange={onDisclosureChange} />
  if (item.type === 'context_rotation' || item.type === 'context_rotation_result') return <ContextRotationRow item={item} itemKey={itemKey} onDisclosureChange={onDisclosureChange} />
  return <ToolCard item={item} itemKey={itemKey} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />
})

function TurnDuration({ itemKey, startedAt, firstTokenAt, finishedAt, inline = false }: { itemKey?: string; startedAt: string; firstTokenAt?: string; finishedAt?: string; inline?: boolean }) {
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    if (finishedAt) return
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [finishedAt])
  const elapsed = (finishedAt ? Date.parse(finishedAt) : now) - Date.parse(startedAt)
  if (inline) {
    return (
      <span className="activity-card-meta" role={finishedAt ? undefined : 'timer'} aria-label="本轮执行时间">
        {firstTokenAt && <time dateTime={firstTokenAt}>{formatClock(firstTokenAt)}</time>}
        {firstTokenAt && ' · '}
        <span>{formatTurnDuration(elapsed)}</span>
      </span>
    )
  }
  return <div className="turn-duration" data-item-id={itemKey} role={finishedAt ? undefined : 'timer'} aria-label="本轮执行时间">{firstTokenAt && <time dateTime={firstTokenAt}>{formatClock(firstTokenAt)}</time>}<span>{formatTurnDuration(elapsed)}</span></div>
}

function ReasoningRow({ item, itemKey, onDisclosureChange }: Pick<Props, 'itemKey' | 'onDisclosureChange'> & { item: Extract<HistoryItem, { type: 'thinking' }> }) {
  const [expanded, setExpanded] = useState(false)
  const guard = useSelectionGuard(onDisclosureChange)
  const collapseBody = useBodyCollapse(expanded ? () => setExpanded(false) : undefined, onDisclosureChange)
  const running = item.status === 'running'
  const text = item.text.trim()
  // 思考中保持单行滚动展示最新文字，新字从右侧进入、旧字在左侧淡出。
  const ticker = running && !expanded
  // 标题行不动，全文在下方展开；展开时标题行的摘要淡出，避免同一句话出现两次。
  return <article className={`timeline-item activity-step reasoning-row status-${item.status}${ticker ? ' is-streaming' : ''}`} data-item-id={itemKey}>
    <button type="button" className="activity-toggle" aria-expanded={expanded} {...guard(() => setExpanded(value => !value))}>
      <StepStatusIcon name="thinking" status={item.status} />
      <span className={`step-title${running ? ' activity-shimmer' : ''}`}>{running ? '思考中' : '思考'}</span>
      <span className={`step-summary reasoning-summary${ticker ? ' is-ticker' : ''}`}>
        {ticker ? <span>{text.replace(/\s+/g, ' ').slice(-400)}</span> : text.split('\n')[0]}
      </span>
      <ExpandChevron expanded={expanded} size={13} />
    </button>
    <Disclosure open={expanded}><div className="activity-expanded reasoning-body" {...collapseBody}>{text}</div></Disclosure>
  </article>
}

function ContextRotationRow({ item, itemKey, onDisclosureChange }: Pick<Props, 'itemKey' | 'onDisclosureChange'> & { item: Extract<HistoryItem, { type: 'context_rotation' | 'context_rotation_result' }> }) {
  const [expanded, setExpanded] = useState(false)
  const content = item.type === 'context_rotation' ? item.userMessages : contextRotationResultText(item)
  const title = item.type === 'context_rotation' ? contextRotationText.completed : item.status === 'failed' ? contextRotationText.failed : content
  const canExpand = content !== '' && content !== title
  const guard = useSelectionGuard(onDisclosureChange)
  const collapseBody = useBodyCollapse(expanded ? () => setExpanded(false) : undefined, onDisclosureChange)
  const status = item.type === 'context_rotation' ? 'completed' : item.status
  return <article className={`timeline-item activity-step timeline-context-rotation status-${status}`} data-item-id={itemKey}>
    <button type="button" className="activity-toggle" aria-expanded={canExpand ? expanded : undefined} aria-disabled={!canExpand}
      {...guard(canExpand ? () => setExpanded(value => !value) : undefined)}>
      <StepStatusIcon name="context_rotation" status={status} />
      <span role="status">{title}</span>
      {canExpand && <ExpandChevron expanded={expanded} size={14} />}
    </button>
    <Disclosure open={expanded}><div className="activity-expanded context-rotation-content" {...collapseBody}><MarkdownBody text={content} /></div></Disclosure>
  </article>
}

/** 截至这条回复或该回合结束时整个会话的累计：平均首 token 时间、平均输出速度、总用量和缓存命中率。 */
function MessageStatsLine({ itemKey, stats, at }: { itemKey?: string; stats?: SessionModelUsage; at?: string }) {
  const parts: string[] = []
  if (stats && stats.ttftRequests > 0) parts.push(`TTFT ${(stats.ttftMs / stats.ttftRequests / 1000).toFixed(1)}s`)
  if (stats && stats.decodeMs > 0) parts.push(`${Math.round(stats.decodeTokens * 1000 / stats.decodeMs)} tok/s`)
  if (stats?.usagePresent) parts.push(`${formatTokenCount(stats.totalTokens)} tokens`)
  // 向下取整：只有全部命中时才显示 100%。
  if (stats?.usagePresent && stats.cacheUsageComplete && stats.inputTokens > 0) parts.push(`${Math.floor(stats.cachedInputTokens / stats.inputTokens * 100)}%`)
  if (at) parts.push(formatClock(at))
  return parts.length ? <p className={itemKey ? 'timeline-item turn-stats' : 'turn-stats'} data-item-id={itemKey} title="平均首 token 时间 · 平均输出速度 · 会话总用量 · 缓存命中率 · 时间">{parts.join(' · ')}</p> : null
}
