import { useLayoutEffect, useMemo, useRef, useState, type MouseEvent } from 'react'
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
      const element = node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(latest.anchorItemId)}"]`)
      if (element !== null) node.scrollTop += element.getBoundingClientRect().top - node.getBoundingClientRect().top - latest.offset
      else { appStore.setViewportAnchor(defaultAnchor()); node.scrollTop = node.scrollHeight }
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

  const saveReadingPosition = (node: HTMLDivElement, element: HTMLElement) => {
    const index = items.findIndex(item => item.key === element.dataset.itemId)
    // 重试会撤回流式片段；用它之前最近的已保存条目记录位置。
    const stable = items.slice(0, index + 1).findLast(item => item.anchorTurnId !== undefined)
      ?? items.slice(index + 1).find(item => item.anchorTurnId !== undefined)
    const target = stable && node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(stable.key)}"]`)
    if (!stable || stable.anchorTurnId === undefined || !target) { appStore.setViewportAnchor(defaultAnchor()); return }
    appStore.setViewportAnchor({ mode: 'anchored', anchorItemId: stable.key, turnId: stable.anchorTurnId,
      offset: target.getBoundingClientRect().top - node.getBoundingClientRect().top })
  }

  const preserveDisclosurePosition = (event: MouseEvent<HTMLDivElement>) => {
    const button = (event.target as Element).closest('button[aria-expanded]')
    const item = button?.closest<HTMLElement>('[data-item-id]')
    const node = viewport.current
    if (item === undefined || item === null || node === null) return
    saveReadingPosition(node, item)
  }

  const onScroll = () => {
    const node = viewport.current
    if (node === null) return
    updateScrollState(node)
    const anchor = appStore.viewportAnchor()
    const floor = Math.max(0, node.scrollHeight - node.clientHeight)
    // 程序化恢复与 Chromium 收缩钳制都保留阅读意图。
    const movedByReader = Math.abs(node.scrollTop - Math.min(observedTop.current, floor)) > 0.5
    observedTop.current = node.scrollTop
    if (!movedByReader) return
    if (floor - node.scrollTop < 32) {
      if (anchor.mode !== 'following') appStore.setViewportAnchor(defaultAnchor())
      return
    }
    const viewportTop = node.getBoundingClientRect().top
    for (const item of node.querySelectorAll<HTMLElement>('[data-item-id]')) {
      if (item.getBoundingClientRect().bottom >= viewportTop) {
        saveReadingPosition(node, item)
        break
      }
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
        <section className="empty-state" aria-live="polite">
          <h2>任务读取失败</h2>
          {state.sessionLoad.error.message && <p>{state.sessionLoad.error.message}</p>}
          {state.sessionLoad.error.recovery && <p className="empty-recovery">{state.sessionLoad.error.recovery}</p>}
          <button type="button" className="secondary-button" onClick={() => appStore.retrySession()}>重试读取</button>
        </section>
      )
    }
    return <div className="conversation-surface" aria-busy="true" />
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
            aria-busy={loadingOlder}
            onClick={() => void appStore.readOlder()}
          >
            加载更早的记录
          </button>
        )}
        {items.map(item => <TimelineItem key={item.fact?.id ?? item.key} item={item} sessionId={sessionId ?? undefined} />)}
      </div>

    </div>
      {awayFromLatest && <button type="button" className="return-to-latest" aria-label="回到最新消息" title="回到最新消息" onClick={returnToLatest}>
        <span aria-hidden="true"><i /><i /><i /></span>
      </button>}
    </div>
  )
}
