import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type MouseEvent } from 'react'
import { ArrowDown } from 'lucide-react'
import { appStore, type AppState } from '../appStore'
import { type TimelineRow } from '../timeline'
import { TimelineItem } from './TimelineItem'
import { defaultAnchor } from '../viewPersistence'
import { BubbleColorPicker, type BubbleKind } from './BubbleColorPicker'

interface Props {
  state: Pick<AppState, 'selectedSessionId' | 'session' | 'sessionLoad' | 'bootstrap'>
  items: TimelineRow[]
}

export function Conversation({ state, items }: Props) {
  const sessionId = state.selectedSessionId
  const currentItems = useRef(items)
  currentItems.current = items
  const itemIdentity = useMemo(() => items.map((item) => item.key).join('\u0000'), [items])
  const viewport = useRef<HTMLDivElement>(null)
  const observedTop = useRef(0)
  const frame = useRef(0)
  const restoredSession = useRef<string | null>(null)
  const reduced = useReducedMotion()
  const [colorPicker, setColorPicker] = useState<{ kind: BubbleKind; x: number; y: number } | null>(null)
  // 右键我的气泡改我的消息配色，右键助手气泡改助手配色；失败气泡保持红色，不响应。
  const openColorPicker = (event: MouseEvent<HTMLElement>) => {
    const bubble = (event.target as Element).closest('.message-bubble:not(.error-bubble)')
    if (!bubble) return
    event.preventDefault()
    setColorPicker({ kind: bubble.closest('.timeline-user') ? 'userBubble' : 'assistantBubble', x: event.clientX, y: event.clientY })
  }
  const disclosurePause = useRef(0)
  const disclosureTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const followLatest = () => {
    const node = viewport.current
    if (!node || frame.current || performance.now() < disclosurePause.current) return
    let previous = performance.now()
    // 保留小数累计值，避免高刷新率下每帧位移被浏览器取整后停在底部前。
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
  useEffect(() => () => {
    cancelAnimationFrame(frame.current)
    if (disclosureTimer.current) clearTimeout(disclosureTimer.current)
    disclosurePause.current = 0
  }, [sessionId])
  const [awayFromLatest, setAwayFromLatest] = useState(false)
  const updateScrollState = (node: HTMLDivElement) => setAwayFromLatest(node.scrollHeight - node.clientHeight - node.scrollTop >= 32)

  useLayoutEffect(() => {
    const node = viewport.current
    if (node === null || sessionId === null) return
    const restoreViewport = () => {
      const latest = appStore.viewportAnchor()
      if (performance.now() < disclosurePause.current) return
      if (latest.mode === 'following') {
        if (restoredSession.current === sessionId && state.session?.runtime.phase !== 'idle') { followLatest(); return }
        // 历史图表完成排版时直接保持贴底，只有执行中的新内容才平滑跟随。
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
    const document = node.querySelector<HTMLElement>('.conversation-document')
    if (document !== null) observer.observe(document)
    return () => { observer.disconnect(); cancelAnimationFrame(frame.current); frame.current = 0 }
  }, [itemIdentity, sessionId, state.session?.runtime.phase])

  const saveReadingPosition = (node: HTMLDivElement, element: HTMLElement) => {
    const anchorElement = element.closest<HTMLElement>('.tool-group') ?? element
    const items = currentItems.current
    const index = items.findIndex(item => item.key === anchorElement.dataset.itemId)
    // 重试会撤回流式片段；用它之前最近的已保存条目记录位置。
    const stable = items.slice(0, index + 1).findLast(item => item.stable)
      ?? items.slice(index + 1).find(item => item.stable)
    const target = stable && node.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(stable.key)}"]`)
    if (!stable || !target) { appStore.setViewportAnchor(defaultAnchor()); return }
    appStore.setViewportAnchor({ mode: 'anchored', anchorItemId: stable.key,
      offset: target.getBoundingClientRect().top - node.getBoundingClientRect().top })
  }

  const preserveDisclosurePosition = useCallback((element: HTMLElement) => {
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
  }, [])


  const onScroll = () => {
    const node = viewport.current
    if (node === null) return
    if (frame.current || performance.now() < disclosurePause.current) return
    updateScrollState(node)
    const anchor = appStore.viewportAnchor()
    const floor = Math.max(0, node.scrollHeight - node.clientHeight)
    // 忽略代码设置 scrollTop 或内容高度收缩导致的被动滚动，只响应用户主动滚动。
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
    followLatest()
    setAwayFromLatest(false)
    node.focus({ preventScroll: true })
  }

  const interruptFollow = () => {
    cancelAnimationFrame(frame.current)
    frame.current = 0
    const node = viewport.current
    if (!node) return
    if (node.scrollHeight - node.clientHeight - node.scrollTop < 32) return
    const top = node.getBoundingClientRect().top
    const first = [...node.querySelectorAll<HTMLElement>('[data-item-id]')].find(item => item.getBoundingClientRect().bottom >= top)
    if (first) saveReadingPosition(node, first)
  }

  // 空白新任务由外层 MainContent 处理；这里只处理已选任务的读取中与读取失败状态。
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
    <div className="conversation-surface"><div className="conversation-scroll" ref={viewport} tabIndex={0} aria-label="任务内容" onScroll={onScroll} onWheel={interruptFollow} onTouchStart={interruptFollow} onPointerDown={interruptFollow} onKeyDown={event => { if (['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' '].includes(event.key)) interruptFollow() }}>
      {/* 切换任务时整页淡入，消息不各自弹出；任务里新出现的消息才逐条弹出。 */}
      <motion.div key={sessionId} className="conversation-document" onContextMenu={openColorPicker}
        initial={reduced ? false : { opacity: 0 }} animate={{ opacity: 1 }} transition={{ duration: .2, ease: 'easeOut' }}>
        <AnimatePresence initial={false}>{items.map(item => <TimelineItem key={item.key} item={item.item} itemKey={item.key} onDisclosureChange={preserveDisclosurePosition} cwd={state.session!.summary.cwd} userHome={state.bootstrap?.userHome} sessionId={sessionId ?? undefined} />)}</AnimatePresence>
      </motion.div>

    </div>
      {colorPicker && <BubbleColorPicker key={`${colorPicker.x},${colorPicker.y}`} {...colorPicker} onClose={() => setColorPicker(null)} />}
      {awayFromLatest && <button type="button" className="return-to-latest" aria-label="回到最新消息" onClick={returnToLatest}>
        <ArrowDown size={16} aria-hidden="true" />
      </button>}
    </div>
  )
}
