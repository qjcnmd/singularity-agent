import { useEffect, useId, useRef, useState, type ClipboardEvent, type DragEvent } from 'react'
import { X } from 'lucide-react'
import type { ImageAttachment } from '../protocol'
import { appStore } from '../appStore'
import { Dialog } from './Dialog'

const accept = '.png,.jpg,.jpeg,.webp,.gif,.bmp'

/** 粘贴、拖入和文件选择共用 File 草稿；编码发生在发送边界。 */
export function useImageInput(add: (images: File[]) => void, disabled = false) {
  const input = useRef<HTMLInputElement>(null)
  const receive = (files: File[]) => { if (!disabled && files.length) add(files) }
  const handlers = {
    onPaste(event: ClipboardEvent) {
      const files = [...event.clipboardData.files].filter(file => file.type.startsWith('image/'))
      if (files.length) { event.preventDefault(); receive(files) }
    },
    onDragOver(event: DragEvent) {
      if (event.dataTransfer.types.includes('Files')) { event.preventDefault(); event.dataTransfer.dropEffect = disabled ? 'none' : 'copy' }
    },
    onDrop(event: DragEvent) {
      if (event.dataTransfer.files.length) { event.preventDefault(); receive([...event.dataTransfer.files]) }
    },
  }
  const picker = <input ref={input} type="file" accept={accept} multiple hidden disabled={disabled} onChange={event => {
      receive([...event.currentTarget.files ?? []]); event.currentTarget.value = ''
    }} />
  return { handlers, picker, open: () => input.current?.click(), disabled }
}

/** 输入与历史只在图片来源上不同，缩略图及大图交互由同一个呈现维护。 */
export function DraftImages({ images, remove }: { images: File[]; remove?: (index: number) => void }) {
  return <div className="image-list">{images.map((image, index) => <ImageTile key={index} name={image.name} file={image} remove={remove && (() => remove(index))} />)}</div>
}

export function AttachedImages({ sessionId, images }: { sessionId: string; images: ImageAttachment[] }) {
  return <div className="image-list">{images.map(image => <SavedImage key={image.id} sessionId={sessionId} image={image} />)}</div>
}

function SavedImage({ sessionId, image }: { sessionId: string; image: ImageAttachment }) {
  const [url, setUrl] = useState<string>()
  const [error, setError] = useState<string | null>(null)
  const [attempt, setAttempt] = useState(0)
  useEffect(() => {
    let active = true
    setError(null)
    void appStore.transport.rpc('session.imageRead', { sessionId, imageId: image.id }).then(
      url => { if (active) setUrl(url) },
      error => { if (active) setError(error instanceof Error ? error.message : String(error)) },
    )
    return () => { active = false }
  }, [sessionId, image.id, attempt])
  return <ImageTile name={image.name} url={url} error={error} retry={() => setAttempt(value => value + 1)} />
}

function ImageTile({ name, url: savedUrl, file, remove, error, retry }: { name: string; url?: string; file?: File; remove?: () => void; error?: string | null; retry?: () => void }) {
  const [localUrl, setLocalUrl] = useState<string>()
  useEffect(() => {
    if (!file) return
    const url = URL.createObjectURL(file)
    setLocalUrl(url)
    return () => URL.revokeObjectURL(url)
  }, [file])
  const url = file ? localUrl : savedUrl
  const [open, setOpen] = useState(false)
  const [broken, setBroken] = useState(false)
  const titleId = useId()
  useEffect(() => setBroken(false), [url])
  return <figure className="image-tile">
    <button type="button" className="image-preview" aria-label={`查看图片 ${name}`} title={name} aria-busy={!url && !error} disabled={!url || broken} onClick={() => setOpen(true)}>
      {url && !broken ? <img src={url} alt={name} onError={() => setBroken(true)} /> : <span>{error || broken ? '图片无法显示' : ''}</span>}
    </button>
    {error && <button type="button" className="image-retry" title={error} onClick={retry}>重试读取</button>}
    {remove && <button type="button" className="image-remove" aria-label={`移除图片 ${name}`} onClick={remove}><X size={14} /></button>}
    <Dialog open={open} onClose={() => setOpen(false)} labelledBy={titleId} className="image-dialog">
      <header><h2 id={titleId}>{name}</h2><button type="button" className="quiet-button" aria-label="关闭图片" onClick={() => setOpen(false)}><X size={20} /></button></header>
      <img src={url} alt={name} />
    </Dialog>
  </figure>
}
