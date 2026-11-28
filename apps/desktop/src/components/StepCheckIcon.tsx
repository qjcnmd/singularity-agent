export function StepCheckIcon({ fresh = false }: { fresh?: boolean }) {
  return (
    <svg className={`step-tick${fresh ? ' is-fresh' : ''}`} width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
      <circle className="step-tick-ring" cx="8" cy="8" r="6.25" />
      <path className="step-tick-check" d="M5.1 8.2l2 2 3.8-4.2" />
    </svg>
  )
}
