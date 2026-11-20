import { ChevronRight } from 'lucide-react'

export function ExpandChevron({ expanded, size = 16, className = '' }: { expanded: boolean; size?: number; className?: string }) {
  return <ChevronRight size={size} className={`expand-chevron${expanded ? ' is-open' : ''}${className ? ` ${className}` : ''}`} aria-hidden="true" />
}
