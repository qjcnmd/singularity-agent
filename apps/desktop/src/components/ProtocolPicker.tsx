import { useId, useRef, useState } from 'react'
import { navigateList, useDismissOnOutside, useTransientFocus } from '../interactions'
import { ExpandChevron } from './ExpandChevron'
import { Disclosure } from './Disclosure'
import type { ProviderApiProtocol } from '../protocol'

const protocols = [['chat', 'Chat Completions'], ['responses', 'Responses']] as const

export function ProtocolPicker({ value, onChange }: { value: ProviderApiProtocol; onChange: (value: ProviderApiProtocol) => void }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const anchor = useRef<HTMLButtonElement>(null)
  const id = useId()
  useDismissOnOutside(root, open, () => setOpen(false))
  useTransientFocus(open, () => setOpen(false), root, node => node.querySelector('[aria-selected="true"]'))
  return <div ref={root} className="protocol-picker" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false)
  }} onKeyDown={event => {
    if (open && navigateList(event.key, [...event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="option"]')])) {
      event.preventDefault()
    } else if (!open && ['ArrowDown', 'ArrowUp'].includes(event.key)) {
      event.preventDefault()
      setOpen(true)
    }
  }}>
    <button ref={anchor} type="button" className="sg-input protocol-trigger" aria-label="API 协议" aria-haspopup="listbox" aria-controls={id} aria-expanded={open} onClick={() => setOpen(value => !value)}>
      <span>{protocols.find(([id]) => id === value)?.[1] ?? `无效协议：${value}`}</span><ExpandChevron expanded={open} size={14} />
    </button>
    <div className="protocol-dropdown"><Disclosure open={open}><div className="protocol-options" id={id} role="listbox" aria-label="API 协议">
      {protocols.map(([id, label]) => <button key={id} type="button" role="option" aria-selected={value === id} autoFocus={value === id} onClick={() => {
        onChange(id)
        setOpen(false)
        anchor.current?.focus()
      }}><span>{label}</span><span aria-hidden="true">{value === id ? '✓' : ''}</span></button>)}
    </div></Disclosure></div>
  </div>
}
