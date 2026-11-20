import { PanelLeft } from 'lucide-react'

export function SidebarToggle({ expanded, controls, onClick }: {
  expanded: boolean
  controls: string
  onClick: () => void
}) {
  return <button type="button" className="icon-button" aria-label={expanded ? '收起侧栏' : '展开侧栏'} aria-expanded={expanded} aria-controls={controls} onClick={onClick}>
    <PanelLeft size={18} aria-hidden="true" />
  </button>
}
