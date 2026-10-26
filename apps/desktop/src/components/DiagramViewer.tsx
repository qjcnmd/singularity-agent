import { useLayoutEffect, useRef, useState, type CSSProperties, type PointerEvent, type WheelEvent } from 'react'
import { X } from 'lucide-react'
import { Dialog } from './Dialog'

type View = { x: number; y: number; scale: number }

const minScale = .2
const maxScale = 8
/** 打开时按查看层铺开，小图最多放大到这个倍数，避免几个节点被放得巨大。 */
const maxFitScale = 1.5
/** 打开时图和查看层边缘之间留的空白。 */
const fitMargin = 40
/** 很扁的图也给查看层留出能拖动缩放的高度。 */
const minStageHeight = 200
/** 按下后移动超过这个距离算拖动，松开时不当作点击空白处关闭。 */
const dragThreshold = 4

/** 居中的大图查看层：滚轮以指针为中心缩放，拖动平移，Esc 或点空白处关闭。 */
export function DiagramViewer({ svg, width, height, style, onClose }: { svg: string; width: number; height: number; style: CSSProperties; onClose: () => void }) {
  const stage = useRef<HTMLDivElement>(null)
  const [view, setView] = useState<View | null>(null)
  const [stageHeight, setStageHeight] = useState<number>()
  const drag = useRef<{ startX: number; startY: number; view: View; moved: boolean } | null>(null)

  useLayoutEffect(() => {
    // 宽度由样式定，高度跟着图走：扁的图不在上下留大片空白，最高不超过 maxStageHeight。
    const { clientWidth } = stage.current!
    const maxStageHeight = Math.min(760, innerHeight * .8)
    const scale = Math.min((clientWidth - fitMargin * 2) / width, (maxStageHeight - fitMargin * 2) / height, maxFitScale)
    const fittedHeight = Math.max(minStageHeight, height * scale + fitMargin * 2)
    setStageHeight(fittedHeight)
    setView({ x: (clientWidth - width * scale) / 2, y: (fittedHeight - height * scale) / 2, scale })
  }, [width, height])

  const zoom = (event: WheelEvent<HTMLDivElement>) => {
    if (!view) return
    const rect = stage.current!.getBoundingClientRect()
    const pointerX = event.clientX - rect.left, pointerY = event.clientY - rect.top
    const scale = Math.min(maxScale, Math.max(minScale, view.scale * Math.exp(-event.deltaY * .0015)))
    // 保持指针下的那一点不动。
    const ratio = scale / view.scale
    setView({ scale, x: pointerX - (pointerX - view.x) * ratio, y: pointerY - (pointerY - view.y) * ratio })
  }
  const press = (event: PointerEvent<HTMLDivElement>) => {
    if (!view || event.button !== 0) return
    event.currentTarget.setPointerCapture(event.pointerId)
    drag.current = { startX: event.clientX, startY: event.clientY, view, moved: false }
  }
  const move = (event: PointerEvent<HTMLDivElement>) => {
    const current = drag.current
    if (!current) return
    const dx = event.clientX - current.startX, dy = event.clientY - current.startY
    if (Math.hypot(dx, dy) > dragThreshold) current.moved = true
    if (current.moved) setView({ ...current.view, x: current.view.x + dx, y: current.view.y + dy })
  }
  const release = (event: PointerEvent<HTMLDivElement>) => {
    const current = drag.current
    drag.current = null
    if (current && !current.moved && !(event.target as Element).closest('.diagram-viewer-canvas svg > g')) onClose()
  }

  return <Dialog open onClose={onClose} labelledBy="diagram-viewer-title" className="diagram-viewer">
    <h2 id="diagram-viewer-title" className="sr-only">图</h2>
    <button type="button" className="icon-button diagram-viewer-close" aria-label="关闭" data-autofocus onClick={onClose}><X size={18} /></button>
    <div ref={stage} className="diagram-viewer-stage" style={{ height: stageHeight }}
      onWheel={zoom} onPointerDown={press} onPointerMove={move} onPointerUp={release} onPointerCancel={() => { drag.current = null }}>
      {view && <div className="diagram-viewer-canvas" style={{ ...style, width, height, transform: `translate(${view.x}px, ${view.y}px) scale(${view.scale})` }}
        dangerouslySetInnerHTML={{ __html: svg }} />}
    </div>
  </Dialog>
}
