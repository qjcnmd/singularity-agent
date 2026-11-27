import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { ArrowDown } from 'lucide-react'
import { appStore, useAppStore, type AppState } from '../appStore'
import { type TimelineRow } from '../timeline'
import { TimelineItem } from './TimelineItem'
import { useConversationAnnotations } from './Annotations'
import { defaultAnchor } from '../viewPersistence'

interface Props {
  state: Pick<AppState, 'selectedSessionId' | 'session' | 'sessionLoad' | 'bootstrap'>
  items: TimelineRow[]
}

function hasRunningBackgroundTask(liveSessions: AppState['liveSessions'], sessionId: string | null): boolean {
  if (sessionId === null) return false
  return liveSessions[sessionId]?.backgroundTasks.some(task => task.state.status === 'running') ?? false
}

export function Conversation({ state, items }: Props) {
  const sessionId = state.selectedSessionId
  const itemIdentity = useMemo(() => items.map((item) => item.key).join('\u0000'), [items])
  const itemIndices = useMemo(() => new Map(itemIdentity ? itemIdentity.split('\u0000').map((key, index) => [key, index]) : []), [itemIdentity])
  const currentRows = useRef({ items, itemIndices })
  currentRows.current = { items, itemIndices }
  const surface = useRef<HTMLDivElement>(null)
  const viewport = useRef<HTMLDivElement>(null)
  const documentNode = useRef<HTMLDivElement>(null)
  const observedTop = useRef(0)
  const frame = useRef(0)
  const anchorFrame = useRef(0)
  const anchorRevision = useRef(0)
  const cancelAnchorScan = useCallback(() => {
    cancelAnimationFrame(anchorFrame.current)
    anchorFrame.current = 0
  }, [])
  const restoredSession = useRef<string | null>(null)
  const reduced = useReducedMotion()
  const disclosurePause = useRef(0)
  const disclosureTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const followLatest = () => {
    const node = viewport.current
    if (!node || frame.current || performance.now() < disclosurePause.current) return
    let previous = performance.now()
    let scrollPosition = node.scrollTop
    const follow = (now: number) => {
      const distance = Math.max(0, node.scrollHeight - node.clientHeight) - scrollPosition
      scrollPosition += reduced || Math.abs(distance) < .5 ? distance : distance * (1 - Math.exp(-(now - previous) / 90))
      node.scrollTop = scrollPosition
      observedTop.current = node.scrollTop
      previous = now
      if (!reduced && Math.abs(distance) >= .5) frame.current = requestAnimationFrame(follow)
      else { frame.current = 0; updateScrollState(node) }
    }
    frame.current = requestAnimationFrame(follow)
  }
  useLayoutEffect(() => () => {
    cancelAnimationFrame(frame.current)
    frame.current = 0
    cancelAnchorScan()
    if (disclosureTimer.current) clearTimeout(disclosureTimer.current)
    disclosurePause.current = 0
  }, [sessionId, cancelAnchorScan])
  const [awayFromLatest, setAwayFromLatest] = useState(false)
  const updateScrollState = (node: HTMLDivElement) => setAwayFromLatest(node.scrollHeight - node.clientHeight - node.scrollTop >= 32)

  useLayoutEffect(() => {
    const node = viewport.current
    if (node === null || sessionId === null) return
    const restoreViewport = () => {
      const latest = appStore.viewportAnchor()
      if ((anchorFrame.current && anchorRevision.current === appStore.viewportAnchorRevision) || performance.now() < disclosurePause.current) return
      if (latest.mode === 'following') {
        if (restoredSession.current === sessionId && state.session?.runtime.phase !== 'idle') { followLatest(); return }
        cancelAnimationFrame(frame.current)
        frame.current = 0
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
    restoredSession.current = sessionId
    const observer = new ResizeObserver(restoreViewport)
    observer.observe(node)
    if (documentNode.current !== null) observer.observe(documentNode.current)
    return () => { observer.disconnect(); cancelAnimationFrame(frame.current); frame.current = 0 }
  }, [itemIdentity, sessionId, state.session?.runtime.phase])

  const saveReadingPosition = (node: HTMLDivElement, element: HTMLElement) => {
    const anchorElement = element.closest<HTMLElement>('.activity-card') ?? element
    const { items, itemIndices } = currentRows.current
    const index = itemIndices.get(anchorElement.dataset.itemId ?? '') ?? -1
    let previous = index
    while (previous >= 0 && !items[previous].stable) previous--
    let next = index + 1
    if (previous < 0) while (next < items.length && !items[next].stable) next++
    const stable = items[previous < 0 ? next : previous]
    const target = stable?.key === anchorElement.dataset.itemId ? anchorElement
      : stable && node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(stable.key)}"]`)
    if (!stable || !target) { appStore.setViewportAnchor(defaultAnchor()); return }
    appStore.setViewportAnchor({ mode: 'anchored', anchorItemId: stable.key,
      offset: target.getBoundingClientRect().top - node.getBoundingClientRect().top })
  }

  const preserveDisclosurePosition = useCallback((element: HTMLElement) => {
    cancelAnchorScan()
    const item = element.closest<HTMLElement>('[data-item-id]')
    const node = viewport.current
    if (item === undefined || item === null || node === null) return
    cancelAnimationFrame(frame.current)
    frame.current = 0
    disclosurePause.current = performance.now() + 400
    saveReadingPosition(node, item)
    if (disclosureTimer.current) clearTimeout(disclosureTimer.current)
    disclosureTimer.current = setTimeout(() => {
      disclosurePause.current = 0
      if (node.scrollHeight - node.clientHeight - node.scrollTop < 32) { appStore.setViewportAnchor(defaultAnchor()); followLatest() }
      updateScrollState(node)
    }, 400)
  }, [cancelAnchorScan])

  const scheduleAnchorScan = useCallback((node: HTMLDivElement) => {
    const revision = appStore.viewportAnchorRevision
    if (anchorFrame.current && anchorRevision.current === revision) return
    cancelAnchorScan()
    anchorRevision.current = revision
    anchorFrame.current = requestAnimationFrame(() => {
      anchorFrame.current = 0
      const conversationDocument = documentNode.current
      if (!conversationDocument || viewport.current !== node || appStore.getSnapshot().selectedSessionId !== sessionId
        || appStore.viewportAnchorRevision !== revision) return
      const children = conversationDocument.children
      const top = node.scrollTop
      if (node.scrollHeight - node.clientHeight - top < 32) {
        if (performance.now() >= disclosurePause.current) appStore.setViewportAnchor(defaultAnchor())
        return
      }
      const viewportTop = node.getBoundingClientRect().top
      let lower = 0
      let upper = children.length
      if (children[upper - 1] && !children[upper - 1].hasAttribute('data-item-id')) upper--
      const count = upper
      while (lower < upper) {
        const middle = (lower + upper) >> 1
        if ((children[middle] as HTMLElement).getBoundingClientRect().bottom >= viewportTop) upper = middle
        else lower = middle + 1
      }
      if (lower < count) saveReadingPosition(node, children[lower] as HTMLElement)
    })
  }, [sessionId, cancelAnchorScan])

  const onScroll = () => {
    const node = viewport.current
    if (node === null) return
    if (frame.current || performance.now() < disclosurePause.current) return
    updateScrollState(node)
    const anchor = appStore.viewportAnchor()
    const floor = Math.max(0, node.scrollHeight - node.clientHeight)
    const movedByReader = Math.abs(node.scrollTop - Math.min(observedTop.current, floor)) > 0.5
    observedTop.current = node.scrollTop
    if (!movedByReader) return
    if (floor - node.scrollTop < 32) {
      cancelAnchorScan()
      if (anchor.mode !== 'following') appStore.setViewportAnchor(defaultAnchor())
      return
    }
    scheduleAnchorScan(node)
  }

  const returnToLatest = () => {
    cancelAnchorScan()
    const node = viewport.current
    if (!node) return
    appStore.setViewportAnchor(defaultAnchor())
    followLatest()
    setAwayFromLatest(false)
    node.focus({ preventScroll: true })
  }

  const interruptFollow = useCallback(() => {
    cancelAnimationFrame(frame.current)
    frame.current = 0
    const node = viewport.current
    if (!node) return
    if (node.scrollHeight - node.clientHeight - node.scrollTop < 32) return
    scheduleAnchorScan(node)
  }, [scheduleAnchorScan])

  const anchorForJump = useCallback((target: HTMLElement) => {
    cancelAnchorScan()
    cancelAnimationFrame(frame.current)
    frame.current = 0
    const node = viewport.current
    if (!node) return
    const item = target.closest<HTMLElement>('[data-item-id]')
    if (item) saveReadingPosition(node, item)
  }, [cancelAnchorScan])

  const { liveSessions } = useAppStore(
    ['liveSessions'],
    (previous, next) => hasRunningBackgroundTask(previous.liveSessions, sessionId) === hasRunningBackgroundTask(next.liveSessions, sessionId),
  )
  const canEdit = state.session?.runtime.phase === 'idle'
    && !hasRunningBackgroundTask(liveSessions, sessionId)
  const [editingMessageId, setEditingMessageId] = useState<string | null>(null)
  useEffect(() => { setEditingMessageId(null) }, [sessionId])
  useEffect(() => { if (!canEdit) setEditingMessageId(null) }, [canEdit])
  const startEdit = useCallback((messageId: string) => { setEditingMessageId(messageId) }, [])
  const cancelEdit = useCallback(() => { setEditingMessageId(null) }, [])

  const { badgeLayer, floatingPopover } = useConversationAnnotations({
    sessionId: state.session ? sessionId : null,
    itemIdentity,
    surfaceRef: surface,
    viewportRef: viewport,
    documentRef: documentNode,
    onJumpAnchor: anchorForJump,
  })

  if (state.session === null) {
    if (state.sessionLoad.status === 'error' && state.sessionLoad.error !== null) {
      return (
        <section className="empty-state" aria-live="polite">
          <h2>任务读取失败</h2>
          {state.sessionLoad.error.message && <p>{state.sessionLoad.error.message}</p>}
          <button type="button" className="secondary-button" onClick={() => appStore.retrySession()}>重试读取</button>
        </section>
      )
    }
    return <div className="conversation-surface" aria-busy="true" />
  }

  return (
    <div className="conversation-surface" ref={surface}><div className="conversation-scroll" ref={viewport} tabIndex={0} aria-label="任务内容" onScroll={onScroll} onWheel={interruptFollow} onTouchStart={interruptFollow} onPointerDown={interruptFollow} onKeyDown={event => { if (['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' '].includes(event.key)) interruptFollow() }}>
      <motion.div key={sessionId} ref={documentNode} className="conversation-document"
        initial={reduced ? false : { opacity: 0 }} animate={{ opacity: 1 }} transition={{ duration: .2, ease: 'easeOut' }}>
        <AnimatePresence initial={false}>{items.map(item => (
          <TimelineItem
            key={item.key}
            item={item.item}
            itemKey={item.key}
            onDisclosureChange={preserveDisclosurePosition}
            cwd={state.session!.summary.cwd}
            userHome={state.bootstrap?.userHome}
            sessionId={sessionId ?? undefined}
            canEdit={canEdit && !item.key.startsWith('pending:')}
            editing={item.item.type === 'message' && item.item.role === 'user' && editingMessageId === item.item.id}
            onEditStart={startEdit}
            onEditCancel={cancelEdit}
          />
        ))}</AnimatePresence>
        {badgeLayer}
      </motion.div>

    </div>
      {floatingPopover}
      {awayFromLatest && <button type="button" className="return-to-latest" aria-label="回到最新消息" onClick={returnToLatest}>
        <ArrowDown size={16} aria-hidden="true" />
      </button>}
    </div>
  )
}
