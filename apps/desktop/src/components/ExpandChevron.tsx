/** 展开/收起指示箭头，展开时旋转 90°。 */
export function ExpandChevron({ expanded, size = 16, className = '' }: { expanded: boolean; size?: number; className?: string }) {
  return <svg className={`expand-chevron${expanded ? ' is-open' : ''}${className ? ` ${className}` : ''}`} width={size} height={size} viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="m8 5 5 5-5 5" /></svg>
}
