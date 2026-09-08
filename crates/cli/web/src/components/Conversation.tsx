import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode, type MouseEvent } from 'react'
import { workbenchStore, type WorkbenchState } from '../store'
import { type TimelineItemModel } from '../timeline'
import { TimelineItem } from './TimelineItem'
import { ActivityOrb } from './ActivityOrb'

interface Props {
  state: WorkbenchState
  items: TimelineItemModel[]
}

export function Conversation({ state, items }: Props) {
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

  const preserveDisclosurePosition = (event: MouseEvent<HTMLDivElement>) => {
    const button = (event.target as Element).closest('button[aria-expanded]')
    const item = button?.closest<HTMLElement>('[data-item-id]')
    const node = viewport.current
    if (item === undefined || item === null || node === null) return
    workbenchStore.setViewportAnchor({ mode: 'anchored', anchorItemId: item.dataset.itemId ?? null,
      offset: item.getBoundingClientRect().top - node.getBoundingClientRect().top })
  }

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
    <div className="conversation-surface"><div className="conversation-scroll" ref={viewport} tabIndex={0} aria-label="任务内容" onScroll={onScroll} onClickCapture={preserveDisclosurePosition}>
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
        ) : items.map(item => <TimelineItem key={item.key} item={item} />)}
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
  return <div className="turn-status" role="status" aria-label="正在运行" aria-live="polite">
    <span className="turn-status-signal" aria-hidden="true">
      <ActivityOrb fast />
      <span className="turn-status-dots">{Array.from({ length: seconds % 3 + 1 }, (_, index) => <span key={index} />)}</span>
    </span>
    {seconds >= 15 && <span className="turn-status-clock" aria-hidden="true">{Math.floor(seconds / 60)}:{String(seconds % 60).padStart(2, '0')}</span>}
  </div>
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
