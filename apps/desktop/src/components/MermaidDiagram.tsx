import { useEffect, useId, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import type { Mermaid } from 'mermaid'
import { Check, Ellipsis, Maximize2 } from 'lucide-react'
import { useSelectionGuard } from '../interactions'
import { DiagramViewer } from './DiagramViewer'
import { Menu } from './Menu'

// mermaid 体积较大，第一次遇到图时才加载，之后复用同一个实例。
let loading: Promise<Mermaid> | null = null
function loadMermaid(): Promise<Mermaid> {
  return loading ??= import('mermaid').then(({ default: mermaid }) => {
    mermaid.initialize({
      startOnLoad: false, securityLevel: 'strict', theme: 'base', look: 'classic', fontFamily: 'inherit',
      // mermaid 12 默认节点文字超过 120px 就换行，节点又窄又容易把单词断开；放宽并加大内边距。
      flowchart: { wrappingWidth: 280, padding: 22, nodeSpacing: 56, rankSpacing: 56 },
      themeVariables: { fontSize: '15px' },
      // 写进图自带的样式表：mermaid 量节点尺寸时也按加粗字排版，不会量完再变宽溢出。
      // 颜色取自容器上的 CSS 变量，明暗主题切换时自动更新；代码里 style/classDef 写的行内样式优先于这里。
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

/** 按内容固定挑一组颜色，同一张图每次打开颜色不变。 */
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

/** 宽图缩到气泡宽度时最多缩到原尺寸的这个比例，再宽就在图内横向滚动，避免字小得看不清。 */
const minimumScale = .6

/** 把 Mermaid 代码画成图，点击或右上角按钮打开大图；加载中或语法无法解析时显示 fallback（原来的代码块）。 */
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
      // 先检查语法：mermaid 渲染失败时会往页面里插一张错误图，检查不过直接保留代码。
      if (!await mermaid.parse(code, { suppressErrors: true })) return
      // mermaid 改完节点尺寸会立即读回来排版；在 .mermaid-stage 里排版，避开减少动态效果样式给所有元素加的过渡（见 surfaces.css）。
      const stage = document.createElement('div')
      stage.className = 'mermaid-stage'
      document.body.append(stage)
      const { svg } = await mermaid.render(id, code, stage).finally(() => stage.remove())
      const [width = 0, height = 0] = /viewBox="[\d.-]+ [\d.-]+ ([\d.]+) ([\d.]+)/.exec(svg)?.slice(1).map(Number) ?? []
      if (active) setDiagram({ svg, width, height })
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
  // 复制失败时把代码展开，代码块自带的复制按钮会显示失败并可重试，也能手动选取。
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
