export function SidebarToggle({ expanded, controls, onClick }: {
  expanded: boolean
  controls: string
  onClick: () => void
}) {
  return <button type="button" className="icon-button sidebar-toggle" aria-label={expanded ? '收起侧栏' : '展开侧栏'} aria-expanded={expanded} aria-controls={controls} onClick={onClick}>
    <svg width="18" height="18" viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><rect x="2.75" y="3.75" width="14.5" height="12.5" rx="3.25" /><path d="M5.25 6.25h2.5v7.5h-2.5z" fill="currentColor" stroke="none" opacity=".55" /></svg>
  </button>
}
