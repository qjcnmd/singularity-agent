import type { DraftAnnotation } from './drafts'

export interface CapturedAnnotation extends Omit<DraftAnnotation, 'id' | 'comment'> {
  range: Range
}

export type { AnnotationJumpTarget }

type AnnotationJumpTarget =
  | { kind: 'draft'; annotationId: string }
  | { kind: 'sent'; messageId: string; quote: string }

export interface TextMap {
  segments: Array<{ node: Text; start: number; end: number }>
  fullText: string
}

const EXCLUDED = '.katex-mathml, .mermaid-diagram, .code-block-header, button'
const CONTEXT_RADIUS = 32
const HIGHLIGHT_KEYS = ['sg-annotation', 'sg-annotation-active', 'sg-annotation-flash'] as const

export const enclosingEl = (node: Node) =>
  node.nodeType === Node.TEXT_NODE ? node.parentElement : node instanceof Element ? node : null

function collectTextMap(root: HTMLElement): TextMap {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      const parent = node.parentElement
      if (!parent || parent.closest(EXCLUDED)) return NodeFilter.FILTER_REJECT
      return node.nodeValue ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP
    },
  })
  const segments: TextMap['segments'] = []
  let offset = 0
  let fullText = ''
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const node = n as Text
    const start = offset
    offset += node.data.length
    segments.push({ node, start, end: offset })
    fullText += node.data
  }
  return { segments, fullText }
}

/** 始终在 Text 节点上设定起止位置，确保与 CSS Custom Highlight API 稳定协作。 */
function rangeFromOffsets(map: TextMap, startOffset: number, endOffset: number): Range | null {
  if (startOffset < 0 || endOffset <= startOffset || endOffset > map.fullText.length) return null
  let startNode: Text | null = null
  let startLocal = 0
  let endNode: Text | null = null
  let endLocal = 0
  for (const seg of map.segments) {
    if (startNode === null && seg.end > startOffset) {
      startNode = seg.node
      startLocal = Math.max(0, startOffset - seg.start)
    }
    if (seg.end >= endOffset) {
      endNode = seg.node
      endLocal = Math.max(0, endOffset - seg.start)
      break
    }
  }
  if (!startNode || !endNode) return null
  const range = document.createRange()
  range.setStart(startNode, startLocal)
  range.setEnd(endNode, endLocal)
  return range
}

/** 仅接受落在同一条已完成助手消息正文内的非空文字选区。 */
export function captureSelectionAnnotation(selection: Selection | null, container: HTMLElement | null): CapturedAnnotation | null {
  if (!selection || selection.isCollapsed || selection.rangeCount === 0 || !container) return null
  const range = selection.getRangeAt(0)
  const body = enclosingEl(range.startContainer)?.closest<HTMLElement>('[data-annotatable-message-id]')
  const endBody = enclosingEl(range.endContainer)?.closest<HTMLElement>('[data-annotatable-message-id]')
  const messageId = body?.dataset.annotatableMessageId
  if (!body || body !== endBody || !messageId || !container.contains(body)) return null

  const map = collectTextMap(body)
  let rawStart = -1
  let rawEnd = -1
  for (const seg of map.segments) {
    const len = seg.node.data.length
    if (range.comparePoint(seg.node, len) < 0) continue
    if (range.comparePoint(seg.node, 0) > 0) break
    const localStart = range.startContainer === seg.node ? range.startOffset : 0
    const localEnd = range.endContainer === seg.node ? range.endOffset : len
    if (localStart >= localEnd) continue
    if (rawStart === -1) rawStart = seg.start + localStart
    rawEnd = seg.start + localEnd
  }
  if (rawStart === -1 || rawEnd <= rawStart) return null

  const slice = map.fullText.slice(rawStart, rawEnd)
  const startOffset = rawStart + (slice.length - slice.trimStart().length)
  const endOffset = rawEnd - (slice.length - slice.trimEnd().length)
  const normalized = rangeFromOffsets(map, startOffset, endOffset)
  if (!normalized) return null
  return {
    messageId,
    quote: map.fullText.slice(startOffset, endOffset),
    startOffset,
    endOffset,
    prefix: map.fullText.slice(Math.max(0, startOffset - CONTEXT_RADIUS), startOffset),
    suffix: map.fullText.slice(endOffset, endOffset + CONTEXT_RADIUS),
    range: normalized,
  }
}

export function resolveAnnotationRange(
  body: HTMLElement,
  anchor: { quote: string; startOffset?: number; endOffset?: number; prefix?: string; suffix?: string },
  cache?: Map<HTMLElement, TextMap>,
): Range | null {
  if (!anchor.quote) return null
  let map = cache?.get(body)
  if (!map) {
    map = collectTextMap(body)
    cache?.set(body, map)
  }
  if (anchor.startOffset !== undefined && anchor.endOffset !== undefined && map.fullText.slice(anchor.startOffset, anchor.endOffset) === anchor.quote) {
    return rangeFromOffsets(map, anchor.startOffset, anchor.endOffset)
  }
  if (anchor.prefix || anchor.suffix) {
    const hit = map.fullText.indexOf(`${anchor.prefix ?? ''}${anchor.quote}${anchor.suffix ?? ''}`)
    if (hit !== -1) {
      const start = hit + (anchor.prefix?.length ?? 0)
      return rangeFromOffsets(map, start, start + anchor.quote.length)
    }
  }
  const target = anchor.startOffset ?? 0
  let best = -1
  let bestDist = Number.POSITIVE_INFINITY
  for (let pos = map.fullText.indexOf(anchor.quote); pos !== -1; pos = map.fullText.indexOf(anchor.quote, pos + 1)) {
    const dist = Math.abs(pos - target)
    if (dist < bestDist) { bestDist = dist; best = pos }
  }
  return best !== -1 ? rangeFromOffsets(map, best, best + anchor.quote.length) : null
}

/** 取选区首个非空矩形，避免换行空矩形导致浮层或角标偏移。 */
export function rangeLeadRect(range: Range): DOMRect | null {
  for (const rect of range.getClientRects()) {
    if (rect.width > 0 && rect.height > 0) return rect
  }
  const fallback = range.getBoundingClientRect()
  return fallback.width > 0 || fallback.height > 0 ? fallback : null
}

export function pointInRange(range: Range, x: number, y: number): boolean {
  for (const r of range.getClientRects()) {
    if (r.width > 0 && r.height > 0 && x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return true
  }
  return false
}

export function syncAnnotationHighlights(groups: { base?: Range[]; active?: Range[]; flash?: Range[] }): void {
  if (typeof CSS === 'undefined' || !('highlights' in CSS)) return
  for (const key of HIGHLIGHT_KEYS) CSS.highlights.delete(key)
  const list = [groups.base, groups.active, groups.flash]
  list.forEach((ranges, priority) => {
    if (!ranges?.length) return
    const highlight = new Highlight(...ranges)
    highlight.priority = priority
    CSS.highlights.set(HIGHLIGHT_KEYS[priority], highlight)
  })
}

export const clearAnnotationHighlights = () => syncAnnotationHighlights({})

const jumpListeners = new Set<(target: AnnotationJumpTarget) => void>()
export const requestAnnotationJump = (target: AnnotationJumpTarget) => { for (const fn of jumpListeners) fn(target) }
export const subscribeAnnotationJump = (fn: (target: AnnotationJumpTarget) => void) => {
  jumpListeners.add(fn)
  return () => { jumpListeners.delete(fn) }
}
