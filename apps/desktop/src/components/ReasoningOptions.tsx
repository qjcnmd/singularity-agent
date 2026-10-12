import type { ReasoningVariant } from '../protocol'

export function ReasoningOptions({ options, selected, onChange }: {
  options: ReasoningVariant[]
  selected: ReasoningVariant[]
  onChange: (variants: ReasoningVariant[]) => void
}) {
  return <fieldset className="model-reasoning-options">
    <legend>可用的思考程度</legend>
    <div className="reasoning-options-grid">{options.map(option => {
      const checked = selected.some(variant => variant.id === option.id)
      return <button type="button" className="reasoning-option" key={option.id} aria-pressed={checked} onClick={() => onChange(checked
        ? selected.filter(variant => variant.id !== option.id) : [...selected, option])}>{option.id}</button>
    })}</div>
  </fieldset>
}
