import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { workbenchStore, type WorkbenchState } from '../store'
import { groupTimelineTools, type TimelineItemModel, type ToolGroupModel } from '../timeline'
import { TimelineItem } from './TimelineItem'
import { Disclosure } from './Disclosure'
import { useSelectionGuard } from '../interactions'
import { ExpandChevron } from './ExpandChevron'

interface Props {
  state: WorkbenchState
  items: TimelineItemModel[]
}

export function Conversation({ state, items }: Props) {
  const rows = useMemo(() => groupTimelineTools(items), [items])
  const itemIdentity = useMemo(() => items.map((item) => item.key).join('\u0000'), [items])
  const viewport = useRef<HTMLDivElement>(null)
  const observedTop = useRef(0)
  const sessionId = state.selectedSessionId

  useLayoutEffect(() => {
    const node = viewport.current
    if (node === null || sessionId === null) return
    const restoreViewport = () => {
      const latest = workbenchStore.viewportAnchor()
      if (latest.mode === 'following') {
        node.scrollTop = node.scrollHeight
        observedTop.current = node.scrollTop
        return
      }
      if (latest.anchorItemId === null) return
      const element = node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(latest.anchorItemId)}"]`)
      if (element !== null) node.scrollTop += element.getBoundingClientRect().top - node.getBoundingClientRect().top - latest.offset
      observedTop.current = node.scrollTop
    }

    restoreViewport()
    const observer = new ResizeObserver(restoreViewport)
    observer.observe(node)
    const document = node.querySelector<HTMLElement>('.conversation-document')
    if (document !== null) observer.observe(document)
    return () => observer.disconnect()
  }, [itemIdentity, sessionId])

  const onScroll = () => {
    const node = viewport.current
    if (node === null) return
    const anchor = workbenchStore.viewportAnchor()
    const floor = Math.max(0, node.scrollHeight - node.clientHeight)
    // Programmatic restoration and browser shrink-clamping preserve reading intent.
    const movedByReader = Math.abs(node.scrollTop - Math.min(observedTop.current, floor)) > 0.5
    observedTop.current = node.scrollTop
    if (!movedByReader) return
    if (floor - node.scrollTop < 32) {
      if (anchor.mode !== 'following') workbenchStore.setViewportAnchor(defaultAnchor)
      return
    }
    const visible = [...node.querySelectorAll<HTMLElement>('[data-item-id]')]
      .find((item) => item.getBoundingClientRect().bottom >= node.getBoundingClientRect().top)
    if (visible !== undefined) {
      workbenchStore.setViewportAnchor({
        mode: 'anchored',
        anchorItemId: visible.dataset.itemId ?? null,
        offset: visible.getBoundingClientRect().top - node.getBoundingClientRect().top,
      })
    }
  }

  if (sessionId === null) {
    return <Empty title="准备做什么？" body="从左侧新建任务。每个任务都有独立、可恢复的任务记录。" />
  }
  if (state.session === null) {
    if (state.sessionLoad.status === 'error' && state.sessionLoad.error !== null) {
      return (
        <Empty title="任务读取失败" body={state.sessionLoad.error.message}>
          <p className="empty-recovery">{state.sessionLoad.error.recovery}</p>
          <button type="button" className="secondary-button" onClick={() => workbenchStore.retrySession()}>重试读取</button>
        </Empty>
      )
    }
    return <Empty title="正在读取任务" busy />
  }

  const sessionOrigin = `session:${sessionId}`
  return (
    <div className="conversation-surface"><div className="conversation-scroll" ref={viewport} tabIndex={0} aria-label="任务内容" onScroll={onScroll}>
      <div className="conversation-document">
        {state.session.history.nextCursor !== null && (
          <button
            type="button"
            className="load-older"
            disabled={workbenchStore.isPending('history.older', sessionOrigin)}
            onClick={() => void workbenchStore.readOlder()}
          >
            {workbenchStore.isPending('history.older', sessionOrigin) ? '正在读取…' : '加载更早的记录'}
          </button>
        )}
        {items.length === 0 ? (
          <div className="conversation-empty-placeholder" />
        ) : rows.map(item => 'tools' in item
          ? <ToolGroup key={item.key} group={item} />
          : <TimelineItem key={item.key} item={item} />)}
        {state.session.runtime.controls.filter(control => control.text !== null && control.channel !== 'cancel' && control.disposition === 'pending' && !state.session!.runtime.pendingControls.some(queued => queued.controlId === control.controlId)).map(control => <article key={control.controlId} className="timeline-item message-item timeline-user pending-message" aria-label="已发送的消息"><div className="user-text">{control.text}</div><small>已发送</small></article>)}
        {(['reserved', 'running'].includes(state.session.runtime.phase)) && (
          <TurnStatus startedAt={state.session.runtime.activeTurn?.startedAt} />
        )}
        {(state.session.runtime.phase === 'stopping' || state.session.runtime.phase === 'compacting') && (
          <div className="turn-activity" role="status">
            {state.session.runtime.phase === 'stopping' ? 'stopping…' : 'compacting…'}
          </div>
        )}
      </div>

    </div>

    </div>
  )
}

function TurnStatus({ startedAt }: { startedAt?: string }) {
  const [mountedAt] = useState(Date.now)
  const start = startedAt ? Date.parse(startedAt) : mountedAt
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [])
  const seconds = Math.max(0, Math.floor((now - start) / 1000))
  return <div className="turn-status" role="status" aria-live="polite">
    Deep diving...
    {seconds >= 15 && <span className="turn-status-clock" aria-hidden="true">{Math.floor(seconds / 60)}:{String(seconds % 60).padStart(2, '0')}</span>}
  </div>
}

function ToolGroup({ group }: { group: ToolGroupModel }) {
  const [expanded, setExpanded] = useState<boolean | null>(null)
  const guard = useSelectionGuard()
  const running = group.tools.some(item => item.requestRunning)
  const failed = group.tools.filter(item => item.status === 'failed').length
  const open = expanded ?? running
  return <section className="tool-group" data-item-id={group.key}>
    <button type="button" className="activity-toggle tool-group-toggle" aria-expanded={open}
      {...guard(() => setExpanded(!open))}>
      <ExpandChevron expanded={open} className="tool-group-chevron" />
      <span>{group.tools.length} 个工具调用</span>
      {running && <span className="item-status">运行中</span>}
      {failed > 0 && <span className="item-status">{failed} 个失败</span>}
    </button>
    <Disclosure open={open}><div className="tool-group-items">
      {group.tools.map(item => <TimelineItem key={item.key} item={item} />)}
    </div></Disclosure>
  </section>
}

function Empty({ title, body, busy = false, children }: { title: string; body?: string; busy?: boolean; children?: ReactNode }) {
  return (
    <section className="empty-state" aria-live="polite">
      {busy && <span className="spinner" aria-hidden="true" />}
      {title && <h2>{title}</h2>}
      {body && <p>{body}</p>}
      {children}
    </section>
  )
}

const defaultAnchor = {
  mode: 'following' as const,
  anchorItemId: null,
  offset: 0,
}
