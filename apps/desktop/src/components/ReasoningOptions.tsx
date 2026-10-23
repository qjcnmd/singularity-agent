
export function ReasoningOptions({ options, selected, onChange }: {
  options: string[]
  selected: string[]
  onChange: (variants: string[]) => void
}) {
  return <fieldset className="model-reasoning-options">
    <legend>可用的思考程度</legend>
    <div className="reasoning-options-grid">{options.map(option => {
      const checked = selected.some(variant => variant === option)
      return <button type="button" className="reasoning-option" key={option} aria-pressed={checked} onClick={() => onChange(checked
        ? selected.filter(variant => variant !== option) : [...selected, option])}>{option}</button>
    })}</div>
  </fieldset>
}
