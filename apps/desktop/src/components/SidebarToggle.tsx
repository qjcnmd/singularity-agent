export function SidebarToggle({ expanded, controls, onClick }: {
  expanded: boolean
  controls: string
  onClick: () => void
}) {
  return <button type="button" className="icon-button sidebar-toggle" aria-label={expanded ? '收起侧栏' : '展开侧栏'} aria-expanded={expanded} aria-controls={controls} onClick={onClick}>
    <svg width="18" height="18" viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><rect x="3" y="4" width="14" height="12" rx="2.5" /><path d="M8 4v12" /></svg>
  </button>
}
