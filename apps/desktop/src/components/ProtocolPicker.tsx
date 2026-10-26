import { Check } from 'lucide-react'
import { useId, useRef, useState } from 'react'
import { navigateList } from '../interactions'
import { ExpandChevron } from './ExpandChevron'
import { Disclosure } from './Disclosure'
import type { ProviderApiProtocol } from '../protocol'

const protocols = [['chat', 'Chat Completions'], ['responses', 'Responses']] as const

export function ProtocolPicker({ value, onChange }: { value: ProviderApiProtocol; onChange: (value: ProviderApiProtocol) => void }) {
  const [open, setOpen] = useState(false)
  const popup = useRef<HTMLDivElement>(null)
  const anchor = useRef<HTMLButtonElement>(null)
  const id = useId()
  return <div className="protocol-picker" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) popup.current!.hidePopover()
  }} onKeyDown={event => {
    if (open && navigateList(event.key, [...event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="option"]')])) {
      event.preventDefault()
    } else if (!open && ['ArrowDown', 'ArrowUp'].includes(event.key)) {
      event.preventDefault()
      anchor.current!.click()
    }
  }}>
    <button ref={anchor} type="button" className="sg-input protocol-trigger" aria-label="API 协议" aria-haspopup="listbox" aria-controls={id} aria-expanded={open} popoverTarget={id}>
      <span>{protocols.find(([id]) => id === value)?.[1] ?? `无效协议：${value}`}</span><ExpandChevron expanded={open} size={14} />
    </button>
    <div ref={popup} id={id} popover="auto" className="protocol-dropdown" onToggle={event => setOpen(event.newState === 'open')}><Disclosure open={open}><div className="protocol-options" role="listbox" aria-label="API 协议">
      {protocols.map(([id, label]) => <button key={id} type="button" role="option" aria-selected={value === id} autoFocus={value === id} onClick={() => {
        onChange(id)
        popup.current!.hidePopover()
        anchor.current?.focus()
      }}><span>{label}</span>{value === id && <Check size={15} aria-hidden="true" />}</button>)}
    </div></Disclosure></div>
  </div>
}
