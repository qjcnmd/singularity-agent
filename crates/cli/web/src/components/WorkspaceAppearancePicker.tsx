import { useRef, type RefObject } from 'react'
import { createPortal } from 'react-dom'
import { BookOpen, Brain, BriefcaseBusiness, ChartNoAxesColumn, Code, Dumbbell, FlaskConical, Flower2, Folder, Globe, GraduationCap, Heart, Leaf, Lightbulb, Mic, Music, Palette, PawPrint, Pencil, Plane, Scale, ShoppingBag, Terminal, Wrench } from 'lucide-react'
import { useTransientFocus, useAnchoredSurface } from '../interactions'
import type { WorkspaceAppearance } from '../store'

const icons = [
  ['folder', '文件夹', Folder], ['book', '书籍', BookOpen], ['graduation', '学习', GraduationCap], ['pencil', '写作', Pencil], ['code', '代码', Code], ['terminal', '终端', Terminal],
  ['music', '音乐', Music], ['palette', '绘画', Palette], ['flower', '花朵', Flower2], ['leaf', '植物', Leaf], ['briefcase', '工作', BriefcaseBusiness], ['chart', '图表', ChartNoAxesColumn],
  ['dumbbell', '运动', Dumbbell], ['scale', '天平', Scale], ['mic', '麦克风', Mic], ['plane', '旅行', Plane], ['globe', '地球', Globe], ['wrench', '工具', Wrench],
  ['paw', '宠物', PawPrint], ['flask', '实验', FlaskConical], ['brain', '思考', Brain], ['heart', '爱心', Heart], ['shopping', '购物', ShoppingBag], ['lightbulb', '灵感', Lightbulb],
] as const
const colors = [['灰黑', '#52525b'], ['红色', '#ef4444'], ['橙色', '#f97316'], ['黄色', '#eab308'], ['绿色', '#22a447'], ['蓝色', '#3b82f6'], ['紫色', '#8b5cf6'], ['粉色', '#ec72ad']] as const
export const defaultWorkspaceAppearance: WorkspaceAppearance = { icon: 'folder', color: '#52525b' }

export function WorkspaceIcon({ appearance = defaultWorkspaceAppearance }: { appearance?: WorkspaceAppearance }) {
  const Icon = (icons.find(([id]) => id === appearance.icon) ?? icons[0])[2]
  return <Icon size={18} strokeWidth={1.7} color={appearance.color} aria-hidden="true" />
}

export function WorkspaceAppearancePicker({ anchor, appearance, onChange, onClose }: {
  anchor: RefObject<HTMLElement | null>
  appearance: WorkspaceAppearance
  onChange: (appearance: WorkspaceAppearance) => void
  onClose: () => void
}) {
  const root = useRef<HTMLDivElement>(null)
  useTransientFocus(true, onClose, root)
  useAnchoredSurface(anchor, root, onClose)

  return createPortal(<div ref={root} className="workspace-appearance-picker" role="dialog" aria-label="项目图标与颜色" onKeyDown={event => {
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key) || !(event.target instanceof HTMLButtonElement)) return
    const group = event.target.closest('[role="group"]')
    if (!group) return
    event.preventDefault()
    const buttons = [...group.querySelectorAll<HTMLButtonElement>('button')]
    const index = buttons.indexOf(event.target)
    const delta = event.key === 'ArrowLeft' ? -1 : event.key === 'ArrowRight' ? 1 : event.key === 'ArrowUp' ? -6 : 6
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + delta + buttons.length) % buttons.length
    buttons[next]?.focus()
  }}>
    <div className="workspace-color-grid" role="group" aria-label="图标颜色">{colors.map(([label, color]) => <button key={color} type="button" aria-label={label} aria-pressed={appearance.color.toLowerCase() === color} style={{ backgroundColor: color }} onClick={() => onChange({ ...appearance, color })} />)}</div>
    <label className="workspace-custom-color"><input type="color" aria-label="自定义图标颜色" value={appearance.color} onChange={event => onChange({ ...appearance, color: event.target.value })} /><span>自定义颜色</span></label>
    <div className="workspace-icon-grid" role="group" aria-label="项目图标">{icons.map(([id, label, Icon]) => <button key={id} type="button" aria-label={label} title={label} aria-pressed={appearance.icon === id} onClick={() => onChange({ ...appearance, icon: id })}><Icon size={19} strokeWidth={1.7} aria-hidden="true" /></button>)}</div>
  </div>, document.body)
}
