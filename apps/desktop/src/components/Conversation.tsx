import { useLayoutEffect, useMemo, useRef, useState, type ReactNode, type MouseEvent } from 'react'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import { type TimelineItemModel } from '../timeline'
import { TimelineItem } from './TimelineItem'
import { defaultAnchor } from '../viewPersistence'

interface Props {
  /** 只声明本组件读取的字段：父级按同一份清单订阅。 */
  state: Pick<AppState, 'selectedSessionId' | 'session' | 'sessionLoad' | 'pendingActions'>
  items: TimelineItemModel[]
}

export function Conversation({ state, items }: Props) {
  const itemIdentity = useMemo(() => items.map((item) => item.key).join('\u0000'), [items])
  const viewport = useRef<HTMLDivElement>(null)
  const observedTop = useRef(0)
  const sessionId = state.selectedSessionId
  const [awayFromLatest, setAwayFromLatest] = useState(false)
  const updateScrollState = (node: HTMLDivElement) => setAwayFromLatest(node.scrollHeight - node.clientHeight - node.scrollTop >= 32)

  useLayoutEffect(() => {
    const node = viewport.current
    if (node === null || sessionId === null) return
    const restoreViewport = () => {
      const latest = appStore.viewportAnchor()
      if (latest.mode === 'following') {
        node.scrollTop = node.scrollHeight
        observedTop.current = node.scrollTop
        updateScrollState(node)
        return
      }
      if (latest.anchorItemId === null) { updateScrollState(node); return }
      const element = node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(latest.anchorItemId)}"]`)
      if (element !== null) node.scrollTop += element.getBoundingClientRect().top - node.getBoundingClientRect().top - latest.offset
      observedTop.current = node.scrollTop
      updateScrollState(node)
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
    appStore.setViewportAnchor({ mode: 'anchored', anchorItemId: item.dataset.itemId ?? null,
      offset: item.getBoundingClientRect().top - node.getBoundingClientRect().top })
  }

  const onScroll = () => {
    const node = viewport.current
    if (node === null) return
    updateScrollState(node)
    const anchor = appStore.viewportAnchor()
    const floor = Math.max(0, node.scrollHeight - node.clientHeight)
    // 程序化恢复与浏览器收缩钳制都保留阅读意图。
    const movedByReader = Math.abs(node.scrollTop - Math.min(observedTop.current, floor)) > 0.5
    observedTop.current = node.scrollTop
    if (!movedByReader) return
    if (floor - node.scrollTop < 32) {
      if (anchor.mode !== 'following') appStore.setViewportAnchor(defaultAnchor())
      return
    }
    // 本次回调只读一次 viewport 顶部：搜索时命中项的矩形直接用于计算偏移。
    const viewportTop = node.getBoundingClientRect().top
    let visible: { id: string | null; top: number } | null = null
    for (const item of node.querySelectorAll<HTMLElement>('[data-item-id]')) {
      const rect = item.getBoundingClientRect()
      if (rect.bottom >= viewportTop) {
        visible = { id: item.dataset.itemId ?? null, top: rect.top }
        break
      }
    }
    if (visible !== null) {
      appStore.setViewportAnchor({
        mode: 'anchored',
        anchorItemId: visible.id,
        offset: visible.top - viewportTop,
      })
    }
  }

  const returnToLatest = () => {
    const node = viewport.current
    if (!node) return
    appStore.setViewportAnchor(defaultAnchor())
    node.scrollTop = node.scrollHeight
    observedTop.current = node.scrollTop
    setAwayFromLatest(false)
    node.focus({ preventScroll: true })
  }

  // 空白新任务由父层 hero 负责；这里只处理已选任务的读取中与读取失败。
  if (state.session === null) {
    if (state.sessionLoad.status === 'error' && state.sessionLoad.error !== null) {
      return (
        <Empty title="任务读取失败" body={state.sessionLoad.error.message}>
          <p className="empty-recovery">{state.sessionLoad.error.recovery}</p>
          <button type="button" className="secondary-button" onClick={() => appStore.retrySession()}>重试读取</button>
        </Empty>
      )
    }
    return <Empty title="正在读取任务" busy />
  }

  const sessionOrigin = actionOrigin.session(sessionId)
  const loadingOlder = state.pendingActions.has(pendingKey('history.older', sessionOrigin))
  return (
    <div className="conversation-surface"><div className="conversation-scroll" ref={viewport} tabIndex={0} aria-label="任务内容" onScroll={onScroll} onClickCapture={preserveDisclosurePosition}>
      <div className="conversation-document">
        {state.session.nextCursor !== null && (
          <button
            type="button"
            className="load-older"
            disabled={loadingOlder}
            onClick={() => void appStore.readOlder()}
          >
            {loadingOlder ? '正在读取…' : '加载更早的记录'}
          </button>
        )}
        {items.map(item => <TimelineItem key={item.key} item={item} />)}
        {(state.session.runtime.phase === 'stopping' || state.session.runtime.phase === 'compacting') && (
          <div className="turn-activity" role="status">
            {state.session.runtime.phase === 'stopping' ? 'stopping…' : 'compacting…'}
          </div>
        )}
      </div>

    </div>
      {awayFromLatest && <button type="button" className="return-to-latest" aria-label="回到最新消息" title="回到最新消息" onClick={returnToLatest}>
        <span aria-hidden="true"><i /><i /><i /></span>
      </button>}
    </div>
  )
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
