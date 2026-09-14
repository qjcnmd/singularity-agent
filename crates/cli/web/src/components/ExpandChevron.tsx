/** Shared Accordion arrow; flips vertically as its panel expands. */
export function ExpandChevron({ expanded, size = 16, className = '' }: { expanded: boolean; size?: number; className?: string }) {
  return <svg className={`expand-chevron${expanded ? ' is-open' : ''}${className ? ` ${className}` : ''}`} width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg>
}
