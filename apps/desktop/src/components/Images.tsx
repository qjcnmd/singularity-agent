import { useEffect, useId, useRef, useState, type ClipboardEvent, type DragEvent } from 'react'
import { FileText, X } from 'lucide-react'
import type { ImageAttachment } from '../protocol'
import { appStore } from '../appStore'
import { Dialog } from './Dialog'

/** 处理粘贴、拖放和选择文件：图片加入草稿 `File` 列表（发送时再编码为 Data URL），
 * 其他文件只记下本地路径。粘贴得到的内存文件没有路径，只能作为图片使用。 */
export function useAttachmentInput(addImages: (images: File[]) => void, addFiles: (paths: string[]) => void, disabled = false) {
  const input = useRef<HTMLInputElement>(null)
  const receive = (files: File[]) => {
    if (disabled) return false
    const images = files.filter(file => file.type.startsWith('image/'))
    const paths = files.filter(file => !file.type.startsWith('image/')).map(file => window.singularity.pathForFile(file)).filter(path => path !== '')
    if (images.length) addImages(images)
    if (paths.length) addFiles(paths)
    return images.length + paths.length > 0
  }
  const handlers = {
    onPaste(event: ClipboardEvent) {
      if (receive([...event.clipboardData.files])) event.preventDefault()
    },
    onDragOver(event: DragEvent) {
      if (event.dataTransfer.types.includes('Files')) { event.preventDefault(); event.dataTransfer.dropEffect = disabled ? 'none' : 'copy' }
    },
    onDrop(event: DragEvent) {
      if (event.dataTransfer.files.length) { event.preventDefault(); receive([...event.dataTransfer.files]) }
    },
  }
  const picker = <input ref={input} type="file" multiple hidden disabled={disabled} onChange={event => {
      receive([...event.currentTarget.files ?? []]); event.currentTarget.value = ''
    }} />
  return { handlers, picker, open: () => input.current?.click(), disabled }
}

export function DraftImages({ images, remove }: { images: File[]; remove: (index: number) => void }) {
  return <div className="image-list">{images.map((image, index) => <ImageTile key={index} name={image.name} file={image} remove={() => remove(index)} />)}</div>
}

export function FileChips({ files, remove }: { files: string[]; remove?: (index: number) => void }) {
  return <div className="file-list">{files.map((path, index) => <span key={path} className="file-chip" title={path}>
    <FileText size={14} strokeWidth={1.7} aria-hidden="true" /><span className="file-chip-name">{path.split(/[\\/]/).at(-1)}</span>
    {remove && <button type="button" className="file-chip-remove" aria-label={`移除附件 ${path}`} onClick={() => remove(index)}><X size={12} /></button>}
  </span>)}</div>
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
    <button type="button" className="image-preview" aria-label={`查看图片 ${name}`} aria-busy={!url && !error} disabled={!url || broken} onClick={() => setOpen(true)}>
      {url && !broken ? <img src={url} alt={name} onError={() => setBroken(true)} /> : <span>{error || broken ? '图片无法显示' : ''}</span>}
    </button>
    {error && <button type="button" className="image-retry" onClick={retry}>重试读取</button>}
    {remove && <button type="button" className="image-remove" aria-label={`移除图片 ${name}`} onClick={remove}><X size={14} /></button>}
    <Dialog open={open} onClose={() => setOpen(false)} labelledBy={titleId} className="image-dialog">
      <header><h2 id={titleId}>{name}</h2><button type="button" className="quiet-button" aria-label="关闭图片" onClick={() => setOpen(false)}><X size={20} /></button></header>
      <img src={url} alt={name} />
    </Dialog>
  </figure>
}
