export function ActivityOrb({ fast = false }: { fast?: boolean }) {
  return <span className={`activity-orb${fast ? ' activity-orb-fast' : ''}`} aria-hidden="true"><span className="orb-cloud" /><span className="orb-light" /></span>
}
