import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { flushSync } from 'react-dom'
import { ArrowDown, X } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, useAppStore, type AppState } from '../appStore'
import type { ChangedFile } from '../protocol'
import { type TimelineRow } from '../timeline'
import { maxTimelineEntryOffset, TimelineItem, type RewindUserMessage } from './TimelineItem'
import { useConversationAnnotations } from './Annotations'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { FileChangeStats } from './EditDiff'
import { ExpandChevron } from './ExpandChevron'
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
  const anchorHint = useRef<{ element: HTMLElement; index: number; top: number } | null>(null)
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
    anchorHint.current = null
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
      const earliestBottom = viewportTop - conversationDocument.getBoundingClientRect().top - maxTimelineEntryOffset - 1
      const mayReachViewport = (index: number) => {
        const child = children[index] as HTMLElement
        return child.offsetTop + child.offsetHeight >= earliestBottom
      }
      let lower = 0
      let upper = children.length
      if (children[upper - 1] && !children[upper - 1].hasAttribute('data-item-id')) upper--
      const count = upper
      const hint = anchorHint.current
      if (hint && children[hint.index] === hint.element && Math.abs(top - hint.top) < node.clientHeight) {
        let index = Math.min(count - 1, Math.max(0, hint.index + Math.sign(top - hint.top)))
        for (let probe = 0; probe < 4 && index >= lower && index < upper; probe++) {
          if (mayReachViewport(index)) { upper = index; index-- }
          else { lower = index + 1; index++ }
        }
      }
      while (lower < upper) {
        const middle = Math.floor((lower + upper) / 2)
        if (mayReachViewport(middle)) upper = middle
        else lower = middle + 1
      }
      for (let index = lower; index < count; index++) {
        const element = children[index] as HTMLElement
        if (element.getBoundingClientRect().bottom >= viewportTop) {
          anchorHint.current = { element, index, top }
          saveReadingPosition(node, element)
          break
        }
      }
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

  const { connection, liveSessions } = useAppStore(
    ['connection', 'liveSessions'],
    (previous, next) => previous.connection === next.connection
      && hasRunningBackgroundTask(previous.liveSessions, sessionId) === hasRunningBackgroundTask(next.liveSessions, sessionId),
  )
  const canRewind = connection === 'ready'
    && state.session?.runtime.phase === 'idle'
    && !hasRunningBackgroundTask(liveSessions, sessionId)
  const [rewindTarget, setRewindTarget] = useState<RewindUserMessage | null>(null)
  useEffect(() => { setRewindTarget(null) }, [sessionId])
  const openRewind = useCallback((message: RewindUserMessage) => { setRewindTarget(message) }, [])
  const closeRewind = useCallback(() => { setRewindTarget(null) }, [])

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
        <AnimatePresence initial={false}>{items.map(item => <TimelineItem key={item.key} item={item.item} itemKey={item.key} onDisclosureChange={preserveDisclosurePosition} cwd={state.session!.summary.cwd} userHome={state.bootstrap?.userHome} sessionId={sessionId ?? undefined} canRewind={canRewind} onRewind={openRewind} />)}</AnimatePresence>
        {badgeLayer}
      </motion.div>

    </div>
      {floatingPopover}
      {awayFromLatest && <button type="button" className="return-to-latest" aria-label="回到最新消息" onClick={returnToLatest}>
        <ArrowDown size={16} aria-hidden="true" />
      </button>}
      {rewindTarget && sessionId && <RewindDialog sessionId={sessionId} message={rewindTarget} onClose={closeRewind} />}
    </div>
  )
}

type ChangedFilesLoad =
  | { status: 'loading' }
  | { status: 'ready'; files: ChangedFile[] }
  | { status: 'error'; message: string }

function RewindDialog({ sessionId, message, onClose }: {
  sessionId: string
  message: RewindUserMessage
  onClose: () => void
}) {
  const origin = actionOrigin.inline('session.rewind', sessionId)
  const { actionErrors, pendingActions } = useAppStore(['actionErrors', 'pendingActions'])
  const busy = pendingActions.has(pendingKey('session.rewind', origin))
  const failure = actionErrors[origin]
  const [restoreFiles, setRestoreFiles] = useState(true)
  const [showFiles, setShowFiles] = useState(false)
  const [filesState, setFilesState] = useState<ChangedFilesLoad>(() =>
    message.hasSnapshot ? { status: 'loading' } : { status: 'ready', files: [] })

  useEffect(() => {
    appStore.clearError(origin)
    return () => appStore.clearError(origin)
  }, [origin, message.turnId])

  useEffect(() => {
    setRestoreFiles(true)
    setShowFiles(false)
    if (!message.hasSnapshot) {
      setFilesState({ status: 'ready', files: [] })
      return
    }
    let active = true
    setFilesState({ status: 'loading' })
    void appStore.transport.rpc('session.changedFiles', { sessionId, turnId: message.turnId }).then(
      result => { if (active) setFilesState({ status: 'ready', files: result.files }) },
      error => { if (active) setFilesState({ status: 'error', message: error instanceof Error ? error.message : String(error) }) },
    )
    return () => { active = false }
  }, [sessionId, message.turnId, message.hasSnapshot])

  const changedFiles = filesState.status === 'ready' ? filesState.files : []
  const shouldRestore = changedFiles.length > 0 && restoreFiles
  const close = () => { if (!busy) onClose() }
  const confirm = async () => {
    const accepted = await appStore.rewindSession(sessionId, message.turnId, shouldRestore, {
      text: message.text,
      images: message.images,
      files: message.files,
      annotations: message.annotations,
    })
    if (accepted) {
      flushSync(onClose)
      appStore.focusComposer()
    }
  }

  return (
    <Dialog open onClose={close} labelledBy="rewind-title" className="confirm-modal">
      <header className="modal-header">
        <h2 id="rewind-title">回到这条消息之前？</h2>
        <button type="button" className="icon-button" disabled={busy} onClick={close} aria-label="关闭"><X size={18} /></button>
      </header>
      <div className="confirm-body">
        <p>将撤回这条消息及之后的对话，并把原话填入底部输入框。</p>
        {filesState.status === 'loading' && <p className="rewind-status" role="status">正在检查文件改动…</p>}
        {filesState.status === 'error' && <p className="rewind-note" role="status">无法检查文件改动（{filesState.message}），本次仅撤回对话。</p>}
        {changedFiles.length > 0 && (
          <div className="rewind-files-block">
            <div className="rewind-checkbox-row">
              <label className="rewind-checkbox">
                <input type="checkbox" checked={restoreFiles} disabled={busy} onChange={event => setRestoreFiles(event.target.checked)} />
                <span>同时还原这之后改动的 {changedFiles.length} 个文件</span>
              </label>
              <button type="button" className="quiet-button rewind-files-toggle" aria-expanded={showFiles} onClick={() => setShowFiles(value => !value)}>
                <span>查看文件</span>
                <ExpandChevron expanded={showFiles} size={12} />
              </button>
            </div>
            <Disclosure open={showFiles}>
              <ul className="rewind-files-list">
                {changedFiles.map(file => (
                  <li key={file.path} className="rewind-file-item" title={file.path}>
                    <span className="rewind-file-path">{file.path}</span>
                    <FileChangeStats file={file} />
                  </li>
                ))}
              </ul>
            </Disclosure>
            <p className="rewind-note">还原会覆盖这之后的手动修改，git push 等外部操作不可撤销。</p>
          </div>
        )}
        {failure && <p className="form-error" role="alert">{failure.message}</p>}
        <footer>
          <button type="button" className="secondary-button" data-autofocus disabled={busy} onClick={close}>取消</button>
          <button type="button" className="primary-button" disabled={busy || filesState.status === 'loading'} onClick={() => void confirm()}>
            {busy ? '处理中…' : '确认'}
          </button>
        </footer>
      </div>
    </Dialog>
  )
}
