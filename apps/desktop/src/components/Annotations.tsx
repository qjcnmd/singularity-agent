import { useCallback, useEffect, useLayoutEffect, useRef, useState, type RefObject } from 'react'
import { Check, MessageSquarePlus, MessageSquareText, Trash2, X } from 'lucide-react'
import { appStore, useAppStore } from '../appStore'
import type { DraftAnnotation } from '../drafts'
import type { MessageAnnotation } from '../protocol'
import {
  captureSelectionAnnotation, clearAnnotationHighlights, enclosingEl, pointInRange, rangeLeadRect,
  requestAnnotationJump, resolveAnnotationRange, subscribeAnnotationJump, syncAnnotationHighlights,
  type CapturedAnnotation, type TextMap,
} from '../annotations'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

interface BadgePos { id: string; index: number; top: number; left: number }
interface PopoverCoords { left: number; top: number; placement: 'above' | 'below'; editorWidth?: number }
type ActiveEditor =
  | { mode: 'create'; captured: CapturedAnnotation; comment: string }
  | { mode: 'edit'; annotationId: string; comment: string }

interface HookProps {
  sessionId: string | null
  itemIdentity: string
  surfaceRef: RefObject<HTMLDivElement | null>
  viewportRef: RefObject<HTMLDivElement | null>
  documentRef: RefObject<HTMLDivElement | null>
  onJumpAnchor: (target: HTMLElement) => void
}

const EMPTY_ANNOTATIONS: DraftAnnotation[] = []
const findBody = (doc: HTMLElement, messageId: string) =>
  doc.querySelector<HTMLElement>(`[data-annotatable-message-id="${CSS.escape(messageId)}"]`)

const badgesEqual = (a: BadgePos[], b: BadgePos[]) =>
  a.length === b.length && a.every((x, i) => x.id === b[i].id && x.index === b[i].index && Math.abs(x.top - b[i].top) <= 0.5 && Math.abs(x.left - b[i].left) <= 0.5)

const coordsEqual = (a: PopoverCoords | null, b: PopoverCoords | null) =>
  a === b || (
    a !== null && b !== null
    && a.placement === b.placement
    && Math.abs(a.left - b.left) <= 0.5
    && Math.abs(a.top - b.top) <= 0.5
    && Math.abs((a.editorWidth ?? 0) - (b.editorWidth ?? 0)) <= 0.5
  )

function computePopoverCoords(range: Range, surface: HTMLElement, viewport: HTMLElement, rightInset: number, w: number, h: number): PopoverCoords | null {
  const lead = rangeLeadRect(range)
  if (!lead) return null
  const surfaceRect = surface.getBoundingClientRect()
  const viewportRect = viewport.getBoundingClientRect()
  const composerTop = surface.closest('.app-main')?.querySelector<HTMLElement>('.composer-region')?.getBoundingClientRect().top
  const safeTop = viewportRect.top + 54
  const safeBottom = composerTop !== undefined ? Math.min(viewportRect.bottom, composerTop) : viewportRect.bottom - 120
  if (lead.bottom < safeTop || lead.top > safeBottom) return null

  const minLeft = 12 + w / 2
  const maxLeft = Math.max(minLeft, Math.max(0, surfaceRect.width - rightInset) - 12 - w / 2)
  const left = Math.min(maxLeft, Math.max(minLeft, lead.left + lead.width / 2 - surfaceRect.left))

  if (lead.top - safeTop >= h + 10) return { left, top: lead.top - surfaceRect.top - 10, placement: 'above' }
  if (lead.bottom + h + 10 <= safeBottom) return { left, top: lead.bottom - surfaceRect.top + 10, placement: 'below' }
  return null
}

function computeBadgeLeft(range: Range, lead: DOMRect, docRect: DOMRect): number | null {
  let rightEdge = lead.right
  const scroller = enclosingEl(range.startContainer)?.closest<HTMLElement>('.markdown-table-scroll, pre')
  if (scroller) {
    const sRect = scroller.getBoundingClientRect()
    if (lead.right < sRect.left || lead.left > sRect.right) return null
    rightEdge = Math.max(sRect.left + 8, Math.min(rightEdge, sRect.right))
  }
  return Math.max(4, Math.min(rightEdge - docRect.left - 3, docRect.width - 20))
}

export function useConversationAnnotations({ sessionId, itemIdentity, surfaceRef, viewportRef, documentRef, onJumpAnchor }: HookProps) {
  const state = useAppStore(['drafts'], (a, b) =>
    (a.drafts === null) === (b.drafts === null)
    && (sessionId === null || a.drafts?.[sessionId]?.annotations === b.drafts?.[sessionId]?.annotations))
  const draftsReady = state.drafts !== null
  const annotations = sessionId !== null ? (state.drafts?.[sessionId]?.annotations ?? EMPTY_ANNOTATIONS) : EMPTY_ANNOTATIONS

  const [trigger, setTrigger] = useState<CapturedAnnotation | null>(null)
  const [editor, setEditor] = useState<ActiveEditor | null>(null)
  const [hoveredId, setHoveredId] = useState<string | null>(null)
  const [badges, setBadges] = useState<BadgePos[]>([])
  const [popoverCoords, setPopoverCoords] = useState<PopoverCoords | null>(null)
  const [flashRange, setFlashRange] = useState<Range | null>(null)

  const live = useRef({ annotations, trigger, editor, hoveredId, flashRange })
  live.current = { annotations, trigger, editor, hoveredId, flashRange }

  const resolvedRangesRef = useRef<Map<string, Range>>(new Map())
  const popoverRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const flashTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const layoutRafRef = useRef(0)
  const hasHighlightsRef = useRef(false)
  const dismissedOnPointerDownRef = useRef(false)
  const focusedKeyRef = useRef<string | null>(null)

  useEffect(() => {
    setTrigger(null); setEditor(null); setHoveredId(null); setFlashRange(null)
    if (flashTimerRef.current) { clearTimeout(flashTimerRef.current); flashTimerRef.current = null }
  }, [sessionId])

  useEffect(() => () => {
    clearAnnotationHighlights()
    if (flashTimerRef.current) clearTimeout(flashTimerRef.current)
    cancelAnimationFrame(layoutRafRef.current)
  }, [])

  const recomputeVisuals = useCallback(() => {
    const doc = documentRef.current
    const surface = surfaceRef.current
    const viewport = viewportRef.current
    const { annotations: list, trigger: curTrigger, editor: curEditor, hoveredId: curHovered, flashRange: curFlash } = live.current

    if (!doc || (list.length === 0 && !curEditor && !curTrigger && !curFlash)) {
      resolvedRangesRef.current.clear()
      if (hasHighlightsRef.current) { clearAnnotationHighlights(); hasHighlightsRef.current = false }
      setBadges(prev => (prev.length === 0 ? prev : []))
      setPopoverCoords(prev => (prev === null ? prev : null))
      return
    }

    const docRect = doc.getBoundingClientRect()
    const ranges = new Map<string, Range>()
    const cache = new Map<HTMLElement, TextMap>()
    const nextBadges: BadgePos[] = []
    const baseRanges: Range[] = []
    const activeRanges: Range[] = []
    const activeEditId = curEditor?.mode === 'edit' ? curEditor.annotationId : null

    const resolveLive = (anchor: CapturedAnnotation) => {
      if (anchor.range.startContainer.isConnected && !anchor.range.collapsed && anchor.range.toString() === anchor.quote) {
        return anchor.range
      }
      const body = findBody(doc, anchor.messageId)
      return body ? resolveAnnotationRange(body, anchor, cache) : null
    }

    list.forEach((item, idx) => {
      const body = findBody(doc, item.messageId)
      const range = body ? resolveAnnotationRange(body, item, cache) : null
      if (!range) return
      ranges.set(item.id, range)
      ;(item.id === activeEditId || item.id === curHovered ? activeRanges : baseRanges).push(range)
      const lead = rangeLeadRect(range)
      const left = lead ? computeBadgeLeft(range, lead, docRect) : null
      if (lead && left !== null) nextBadges.push({ id: item.id, index: idx + 1, top: lead.top - docRect.top - 9, left })
    })

    const createRange = curEditor?.mode === 'create' ? resolveLive(curEditor.captured) : null
    if (createRange) activeRanges.push(createRange)

    resolvedRangesRef.current = ranges
    const anyHighlight = baseRanges.length > 0 || activeRanges.length > 0 || curFlash !== null
    if (anyHighlight || hasHighlightsRef.current) {
      syncAnnotationHighlights({ base: baseRanges, active: activeRanges, flash: curFlash ? [curFlash] : undefined })
      hasHighlightsRef.current = anyHighlight
    }
    setBadges(prev => (badgesEqual(prev, nextBadges) ? prev : nextBadges))

    const anchorRange = curEditor?.mode === 'create' ? createRange
      : curEditor?.mode === 'edit' ? (ranges.get(curEditor.annotationId) ?? null)
      : curTrigger ? resolveLive(curTrigger) : null

    if (!surface || !viewport || !anchorRange) {
      setPopoverCoords(prev => (prev === null ? prev : null))
      return
    }
    const rightInset = Number.parseFloat(getComputedStyle(viewport).paddingRight) || 0
    const available = Math.max(0, surface.clientWidth - rightInset - 24)
    const editorWidth = Math.min(360, available > 0 ? available : 220)
    const w = curEditor ? editorWidth : 76
    const hasCurrentPopover = popoverRef.current !== null && Boolean(popoverRef.current.querySelector(curEditor ? '.annotation-editor' : '.annotation-trigger-button'))
    const h = hasCurrentPopover ? popoverRef.current!.offsetHeight : (curEditor ? 52 : 34)
    const coords = computePopoverCoords(anchorRange, surface, viewport, rightInset, w, h)
    const nextCoords = coords ? (curEditor ? { ...coords, editorWidth } : coords) : null
    setPopoverCoords(prev => (coordsEqual(prev, nextCoords) ? prev : nextCoords))
  }, [documentRef, surfaceRef, viewportRef])

  const scheduleRecompute = useCallback(() => {
    cancelAnimationFrame(layoutRafRef.current)
    layoutRafRef.current = requestAnimationFrame(recomputeVisuals)
  }, [recomputeVisuals])

  useLayoutEffect(() => { recomputeVisuals() }, [annotations, sessionId, itemIdentity, trigger, editor, hoveredId, flashRange, recomputeVisuals])
  useEffect(() => {
    if (editor?.mode === 'edit' && !annotations.some(item => item.id === editor.annotationId)) setEditor(null)
  }, [annotations, editor])

  useEffect(() => {
    const doc = documentRef.current
    const viewport = viewportRef.current
    if (!doc || !viewport || sessionId === null) return
    const active = () => {
      const s = live.current
      return s.annotations.length > 0 || s.editor !== null || s.trigger !== null || s.flashRange !== null
    }
    const onChange = () => { if (active()) scheduleRecompute() }
    const ro = new ResizeObserver(onChange)
    ro.observe(doc); ro.observe(viewport)
    const mo = new MutationObserver(records => {
      if (!active()) return
      if (records.some(r => !(r.target instanceof Element ? r.target : r.target.parentElement)?.closest('.annotation-badge-layer'))) scheduleRecompute()
    })
    mo.observe(doc, { childList: true, subtree: true })
    viewport.addEventListener('scroll', onChange, { passive: true })
    doc.addEventListener('scroll', onChange, { passive: true, capture: true })
    window.addEventListener('resize', onChange, { passive: true })
    return () => {
      ro.disconnect(); mo.disconnect()
      viewport.removeEventListener('scroll', onChange)
      doc.removeEventListener('scroll', onChange, true)
      window.removeEventListener('resize', onChange)
    }
  }, [documentRef, viewportRef, sessionId, scheduleRecompute])

  useEffect(() => {
    const doc = documentRef.current
    if (!doc) return
    let raf = 0
    const inspect = () => {
      cancelAnimationFrame(raf)
      raf = requestAnimationFrame(() => { if (!live.current.editor) setTrigger(captureSelectionAnnotation(window.getSelection(), doc)) })
    }
    const clearDismissed = () => { if (live.current.editor === null) dismissedOnPointerDownRef.current = false }
    const onPointerUp = (e: PointerEvent) => { if (!(e.target instanceof Node && popoverRef.current?.contains(e.target))) inspect() }
    const onKeyUp = (e: KeyboardEvent) => { if (e.shiftKey || e.key === 'a' || e.key === 'A') inspect() }
    const onSelectionChange = () => {
      if (live.current.editor) return
      const sel = window.getSelection()
      if (!sel || sel.isCollapsed || sel.toString().trim() === '') setTrigger(null)
    }
    const onDocClick = (e: MouseEvent) => {
      if (dismissedOnPointerDownRef.current) { dismissedOnPointerDownRef.current = false; return }
      if (live.current.editor) return
      const sel = window.getSelection()
      if (sel && !sel.isCollapsed && sel.toString() !== '') return
      if (e.target instanceof Element && e.target.closest('a, button, .file-link, .annotation-badge')) return
      for (const [id, range] of resolvedRangesRef.current) {
        if (!pointInRange(range, e.clientX, e.clientY)) continue
        const target = live.current.annotations.find(item => item.id === id)
        if (!target) continue
        setTrigger(null); setEditor({ mode: 'edit', annotationId: target.id, comment: target.comment }); return
      }
    }
    document.addEventListener('pointerdown', clearDismissed, true)
    document.addEventListener('pointerup', onPointerUp)
    document.addEventListener('keyup', onKeyUp)
    document.addEventListener('selectionchange', onSelectionChange)
    doc.addEventListener('click', onDocClick)
    return () => {
      cancelAnimationFrame(raf)
      document.removeEventListener('pointerdown', clearDismissed, true)
      document.removeEventListener('pointerup', onPointerUp)
      document.removeEventListener('keyup', onKeyUp)
      document.removeEventListener('selectionchange', onSelectionChange)
      doc.removeEventListener('click', onDocClick)
    }
  }, [documentRef, sessionId])

  const editorKey = editor ? (editor.mode === 'create' ? 'create' : editor.annotationId) : null
  const popoverMounted = popoverCoords !== null
  useLayoutEffect(() => {
    if (popoverMounted) recomputeVisuals()
  }, [popoverMounted, editorKey, recomputeVisuals])
  useLayoutEffect(() => {
    if (!editorKey) { focusedKeyRef.current = null; return }
    if (!popoverMounted || focusedKeyRef.current === editorKey || !inputRef.current) return
    focusedKeyRef.current = editorKey
    inputRef.current.focus({ preventScroll: true })
    const end = inputRef.current.value.length
    inputRef.current.setSelectionRange(end, end)
  }, [editorKey, popoverMounted])

  useEffect(() => {
    if (!editor) return
    const onOutside = (e: PointerEvent) => {
      if ((e.target instanceof Node && popoverRef.current?.contains(e.target)) || (e.target instanceof Element && e.target.closest('.annotation-badge'))) return
      dismissedOnPointerDownRef.current = true
      setEditor(null)
    }
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') { e.preventDefault(); setEditor(null) } }
    document.addEventListener('pointerdown', onOutside, true)
    document.addEventListener('keydown', onKey)
    return () => { document.removeEventListener('pointerdown', onOutside, true); document.removeEventListener('keydown', onKey) }
  }, [editor])

  useEffect(() => subscribeAnnotationJump(target => {
    const doc = documentRef.current
    const viewport = viewportRef.current
    if (!doc || !viewport) return
    const scrollTo = (anchorEl: HTMLElement, range: Range | null) => {
      onJumpAnchor(anchorEl)
      const scroller = range ? enclosingEl(range.startContainer)?.closest<HTMLElement>('.markdown-table-scroll, pre') : null
      if (scroller && range && scroller.scrollWidth > scroller.clientWidth) {
        const lead = rangeLeadRect(range)
        if (lead) {
          const sRect = scroller.getBoundingClientRect()
          if (lead.left < sRect.left + 8 || lead.right > sRect.right - 8) {
            scroller.scrollTo({ left: Math.max(0, scroller.scrollLeft + (lead.left - sRect.left) - sRect.width * 0.25), behavior: 'auto' })
          }
        }
      }
      const rect = (range ? rangeLeadRect(range) : null) ?? anchorEl.getBoundingClientRect()
      const vRect = viewport.getBoundingClientRect()
      const top = Math.max(0, viewport.scrollTop + (rect.top - vRect.top) - vRect.height * 0.34)
      viewport.scrollTo({ top, behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' })
    }
    if (target.kind === 'draft') {
      const item = live.current.annotations.find(x => x.id === target.annotationId)
      if (!item) return
      const body = findBody(doc, item.messageId)
      const range = resolvedRangesRef.current.get(item.id)
      if (body) scrollTo(body, range ?? null)
      setTrigger(null); setEditor({ mode: 'edit', annotationId: item.id, comment: item.comment })
      return
    }
    const body = findBody(doc, target.messageId)
    if (!body) return
    const range = resolveAnnotationRange(body, { quote: target.quote })
    scrollTo(body, range)
    if (range) {
      setFlashRange(range)
      if (flashTimerRef.current) clearTimeout(flashTimerRef.current)
      flashTimerRef.current = setTimeout(() => { flashTimerRef.current = null; setFlashRange(null) }, 1500)
    }
  }), [documentRef, viewportRef, onJumpAnchor])

  const commitEditor = () => {
    if (!editor || !draftsReady) return
    const comment = editor.comment.trim()
    if (editor.mode === 'create') {
      const { messageId, quote, startOffset, endOffset, prefix, suffix } = editor.captured
      appStore.addAnnotation({ messageId, quote, comment, startOffset, endOffset, prefix, suffix })
    } else appStore.updateAnnotation(editor.annotationId, comment)
    setEditor(null)
  }

  const badgeLayer = badges.length > 0 ? (
    <div className="annotation-badge-layer">
      {badges.map(b => {
        const active = hoveredId === b.id || (editor?.mode === 'edit' && editor.annotationId === b.id)
        return (
          <button key={b.id} type="button" className={`annotation-badge${active ? ' is-active' : ''}`}
            style={{ top: b.top, left: b.left }} aria-label={`编辑第 ${b.index} 条批注`} title={`第 ${b.index} 条批注`}
            onMouseEnter={() => setHoveredId(b.id)}
            onMouseLeave={() => setHoveredId(cur => (cur === b.id ? null : cur))}
            onClick={e => {
              e.stopPropagation()
              const target = annotations.find(x => x.id === b.id)
              if (target) { setTrigger(null); setEditor({ mode: 'edit', annotationId: target.id, comment: target.comment }) }
            }}
          >{b.index}</button>
        )
      })}
    </div>
  ) : null

  const floatingPopover = popoverCoords && (editor || trigger) ? (
    <div ref={popoverRef} className={`annotation-popover is-${popoverCoords.placement}`}
      style={{ left: popoverCoords.left, top: popoverCoords.top }}
      role={editor ? 'dialog' : undefined} aria-label={editor ? (editor.mode === 'create' ? '添加批注' : '编辑批注') : undefined}>
      {editor ? (
        <div className="annotation-editor" style={popoverCoords.editorWidth ? { width: popoverCoords.editorWidth } : undefined}>
          <textarea ref={inputRef} className="annotation-input" rows={1} value={editor.comment} placeholder="添加可选批注…" aria-label="批注内容"
            onChange={e => { const v = e.target.value; setEditor(cur => (cur ? { ...cur, comment: v } : null)) }}
            onKeyDown={e => { if (!e.nativeEvent.isComposing && e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); commitEditor() } }}
          />
          {editor.mode === 'edit' && (
            <button type="button" className="annotation-action-button is-danger" aria-label="删除批注" title="删除批注"
              onClick={() => { appStore.removeAnnotation(editor.annotationId); setEditor(null) }}>
              <Trash2 size={14} strokeWidth={1.8} aria-hidden="true" />
            </button>
          )}
          <button type="button" className="annotation-confirm-button" disabled={!draftsReady} aria-label={editor.mode === 'create' ? '保存批注' : '更新批注'} title="保存（Enter）" onClick={commitEditor}>
            <Check size={14} strokeWidth={2.2} aria-hidden="true" />
          </button>
        </div>
      ) : (
        <button type="button" className="annotation-trigger-button" onMouseDown={e => e.preventDefault()} onClick={() => {
          if (!trigger) return
          const captured = trigger
          window.getSelection()?.removeAllRanges()
          setTrigger(null)
          setEditor({ mode: 'create', captured, comment: '' })
        }}>
          <MessageSquarePlus size={14} strokeWidth={1.8} aria-hidden="true" /><span>批注</span>
        </button>
      )}
    </div>
  ) : null

  return { badgeLayer, floatingPopover }
}

export function DraftAnnotationsTray({ annotations, remove, clear }: { annotations: DraftAnnotation[]; remove: (id: string) => void; clear: () => void }) {
  const [expanded, setExpanded] = useState(false)
  useEffect(() => { if (annotations.length === 0) setExpanded(false) }, [annotations.length])
  if (annotations.length === 0) return null
  return (
    <div className="draft-annotations">
      <div className="draft-annotations-bar">
        <button type="button" className="draft-annotations-toggle" aria-expanded={expanded} onClick={() => setExpanded(v => !v)}>
          <MessageSquareText size={13} strokeWidth={1.8} aria-hidden="true" /><span>{annotations.length} 条批注</span><ExpandChevron expanded={expanded} size={12} />
        </button>
        <button type="button" className="draft-annotations-clear" aria-label="清空全部批注" onClick={clear}>清空</button>
      </div>
      <Disclosure open={expanded}>
        <div className="draft-annotations-list">
          {annotations.map((item, idx) => (
            <div key={item.id} className="draft-annotation-row">
              <button type="button" className="draft-annotation-jump" onClick={() => requestAnnotationJump({ kind: 'draft', annotationId: item.id })}>
                <span className="draft-annotation-index">{idx + 1}</span>
                <span className="draft-annotation-content">
                  <span className="draft-annotation-quote">“{item.quote}”</span>
                  {item.comment && <span className="draft-annotation-comment">{item.comment}</span>}
                </span>
              </button>
              <button type="button" className="draft-annotation-remove" aria-label={`移除第 ${idx + 1} 条批注`} onClick={() => remove(item.id)}>
                <X size={12} aria-hidden="true" />
              </button>
            </div>
          ))}
        </div>
      </Disclosure>
    </div>
  )
}

export function SentAnnotations({ annotations, defaultExpanded = false }: { annotations: MessageAnnotation[]; defaultExpanded?: boolean }) {
  const [expanded, setExpanded] = useState(defaultExpanded)
  if (annotations.length === 0) return null
  return (
    <div className="sent-annotations">
      <button type="button" className="sent-annotations-pill" aria-expanded={expanded} onClick={() => setExpanded(v => !v)}>
        <MessageSquareText size={12.5} strokeWidth={1.8} aria-hidden="true" />
        <span>{annotations.length} 条批注</span>
        <ExpandChevron expanded={expanded} size={12} />
      </button>
      <Disclosure open={expanded}>
        <div className="sent-annotations-list">
          {annotations.map((item, idx) => (
            <div key={`${item.messageId}:${idx}`} className="sent-annotation-item">
              <button type="button" className="sent-annotation-quote" title="定位到原助手回复"
                onClick={() => requestAnnotationJump({ kind: 'sent', messageId: item.messageId, quote: item.quote })}>
                <span className="sent-annotation-index">{idx + 1}</span>
                <span className="sent-annotation-quote-text">{item.quote}</span>
              </button>
              {item.comment && <div className="sent-annotation-comment">{item.comment}</div>}
            </div>
          ))}
        </div>
      </Disclosure>
    </div>
  )
}
