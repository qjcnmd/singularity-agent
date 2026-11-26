import { useEffect, useId, useRef, useState, type CSSProperties, type MouseEvent, type ReactNode } from 'react'
import type { Mermaid } from 'mermaid'
import { Check, Ellipsis, Maximize2 } from 'lucide-react'
import { useSelectionGuard } from '../interactions'
import { DiagramViewer, interceptSvgLinkClick } from './DiagramViewer'
import { Menu } from './Menu'

let loading: Promise<Mermaid> | null = null
function loadMermaid(): Promise<Mermaid> {
  return loading ??= import('mermaid').then(({ default: mermaid }) => {
    mermaid.initialize({
      startOnLoad: false, suppressErrorRendering: true, securityLevel: 'strict', theme: 'base', look: 'classic', fontFamily: 'inherit',
      flowchart: { wrappingWidth: 280, padding: 22, nodeSpacing: 56, rankSpacing: 56 },
      themeVariables: { fontSize: '15px' },
      // 写进图自带的样式表：mermaid 量节点尺寸时也按加粗字排版，不会量完再变宽溢出。
      themeCSS: `.node rect, .node polygon, .node circle { fill: var(--diagram-fill); stroke: var(--diagram-border); }
        .node rect { rx: 16px; ry: 16px; }
        .nodeLabel, .node .label, .node text { color: var(--diagram-text); fill: var(--diagram-text); font-weight: 600; }
        .edgePath .path, .flowchart-link { stroke: var(--mermaid-line); }
        .marker { fill: var(--mermaid-line); stroke: var(--mermaid-line); }
        .edgeLabel, .edgeLabel p, .edgeLabel span { color: var(--mermaid-ink-soft); background: var(--mermaid-edge-bg); }
        .edgeLabel rect, .labelBkg { fill: var(--mermaid-edge-bg); background-color: var(--mermaid-edge-bg); }
        .cluster rect { fill: var(--mermaid-cluster-bg); stroke: var(--mermaid-cluster-border); }
        .cluster-label .nodeLabel, .cluster-label span { color: var(--mermaid-ink); }`,
    })
    return mermaid
  }).catch(error => {
    loading = null
    throw error
  })
}

const palettes = [
  { light: ['#fde8f1', '#f3c6da', '#9b2f5e'], dark: ['#3b1f2d', '#733855', '#f9bdd6'] },
  { light: ['#e7f0ff', '#c4d8fb', '#1f4f9c'], dark: ['#1d2b44', '#375587', '#b4d0ff'] },
  { light: ['#e5f6ec', '#bfe5cd', '#1f6b40'], dark: ['#1b3326', '#33664b', '#a8e3bf'] },
  { light: ['#fdf1dc', '#f1d5a3', '#8a5a12'], dark: ['#362916', '#6b5228', '#f5d293'] },
  { light: ['#efe9fd', '#d4c8f6', '#5b3aa6'], dark: ['#2a223f', '#52407c', '#cfc1fb'] },
]

function paletteStyle(code: string): CSSProperties {
  const hash = [...code].reduce((sum, character) => (sum * 31 + character.charCodeAt(0)) >>> 0, 7)
  const chosen = palettes[hash % palettes.length]
  return {
    '--diagram-fill-light': chosen.light[0],
    '--diagram-border-light': chosen.light[1],
    '--diagram-text-light': chosen.light[2],
    '--diagram-fill-dark': chosen.dark[0],
    '--diagram-border-dark': chosen.dark[1],
    '--diagram-text-dark': chosen.dark[2],
  } as CSSProperties
}

const minimumScale = .6

export function MermaidDiagram({ code, fallback }: { code: string; fallback: ReactNode }) {
  const id = `mermaid-${useId().replace(/[^\w-]/g, '')}`
  const [diagram, setDiagram] = useState<{ svg: string; width: number; height: number } | null>(null)
  const [viewing, setViewing] = useState(false)
  const [showCode, setShowCode] = useState(false)
  const [menuOpen, setMenuOpen] = useState(false)
  const [copied, setCopied] = useState(false)
  const menuButton = useRef<HTMLButtonElement>(null)
  const selectionGuard = useSelectionGuard()
  useEffect(() => {
    let active = true
    setDiagram(null)
    void loadMermaid().then(async mermaid => {
      if (!active) return
      const stage = document.createElement('div')
      stage.className = 'mermaid-stage'
      document.body.append(stage)
      try {
        const { svg } = await mermaid.render(id, code, stage)
        const [width = 0, height = 0] = /viewBox="[\d.-]+ [\d.-]+ ([\d.]+) ([\d.]+)/.exec(svg)?.slice(1).map(Number) ?? []
        if (active) setDiagram({ svg, width, height })
      } finally {
        stage.remove()
      }
    }).catch(() => {})
    return () => { active = false }
  }, [code, id])
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 1200)
    return () => clearTimeout(timer)
  }, [copied])
  if (diagram === null) return fallback
  const open = () => setViewing(true)
  const copy = () => { void navigator.clipboard.writeText(code).then(() => setCopied(true), () => setShowCode(true)) }
  return <>
    <div className="mermaid-block" style={paletteStyle(code)}>
      <div className="mermaid-tools">
        <button ref={menuButton} type="button" className="icon-button" aria-label={copied ? '已复制代码' : '图的更多操作'} aria-haspopup="menu" aria-expanded={menuOpen}
          onClick={() => setMenuOpen(true)}>{copied ? <Check size={16} /> : <Ellipsis size={16} />}</button>
        <button type="button" className="icon-button" aria-label="放大查看图" onClick={open}><Maximize2 size={15} /></button>
      </div>
      {/* securityLevel: 'strict' 已经清理过 SVG 里的脚本和事件属性。 */}
      <div className="mermaid-diagram" role="button" tabIndex={0} aria-label="放大查看图"
        style={{ '--diagram-min-width': `${Math.round(diagram.width * minimumScale)}px` } as CSSProperties}
        onClickCapture={(event: MouseEvent<HTMLDivElement>) => interceptSvgLinkClick(event)}
        {...selectionGuard(open)}
        onKeyDown={event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); open() } }}
        dangerouslySetInnerHTML={{ __html: diagram.svg }} />
    </div>
    {showCode && fallback}
    {menuOpen && <Menu anchor={menuButton} label="图的更多操作" onClose={() => setMenuOpen(false)} entries={[
      { id: 'copy', label: '复制代码', onSelect: copy },
      { id: 'code', label: showCode ? '隐藏代码' : '查看代码', onSelect: () => setShowCode(shown => !shown) },
    ]} />}
    {viewing && <DiagramViewer {...diagram} style={paletteStyle(code)} onClose={() => setViewing(false)} />}
  </>
}
