import { useSelectionGuard } from '../interactions'
import { workbenchStore, type DirectoryPickerState } from '../store'
import { Dialog } from './Dialog'

export function DirectoryPicker({ picker }: { picker: DirectoryPickerState }) {
  const selectionGuard = useSelectionGuard()
  const origin = `directory:${picker.path ?? 'root'}`
  const adding = picker.path !== null && workbenchStore.isPending('workspace.add', `directory:${picker.path}`)
  const addError = workbenchStore.getSnapshot().actionErrors[origin]
  return (
    <Dialog open={picker.open} onClose={() => workbenchStore.closeDirectoryPicker()} labelledBy="directory-title" className="directory-picker">
      <header className="modal-header">
        <div><span className="eyebrow">项目</span><h2 id="directory-title">选择项目文件夹</h2></div>
        <button type="button" className="icon-button" data-autofocus onClick={() => workbenchStore.closeDirectoryPicker()} aria-label="关闭">×</button>
      </header>
      <p className="current-path">{picker.path ?? '此电脑'}</p>
      {picker.error !== null && (
        <div className="inline-error" role="alert">
          <strong>{picker.error.message}</strong>
          <span>{picker.error.recovery}</span>
          <button type="button" className="quiet-button" {...selectionGuard(() => { void workbenchStore.browseDirectory(picker.path) })}>重试</button>
        </div>
      )}
      <div className="directory-list" aria-busy={picker.loading} aria-live="polite">
        {picker.loading && <p className="muted">正在读取下一层文件夹…</p>}
        {!picker.loading && picker.error === null && picker.entries.length === 0 && <p className="muted">这里没有可选择的子文件夹。</p>}
        {!picker.loading && picker.entries.map((entry) => (
          <button
            type="button"
            key={`${entry.kind}:${entry.path}`}
            className="directory-row"
            {...selectionGuard(() => { void workbenchStore.browseDirectory(entry.path) })}
          >
            <span aria-hidden="true">{entry.kind === 'parent' ? '↰' : entry.kind === 'root' ? '▣' : '▱'}</span>
            <strong>{entry.name}</strong>
            <small>{entry.path}</small>
          </button>
        ))}
      </div>
      <footer className="modal-footer">
        <p>这里只登记所选目录；其中的文件不会被移动。项目决定工作上下文，不是权限边界。</p>
        {addError !== undefined && (
          <div className="inline-error" role="alert">
            <strong>{addError.message}</strong>
            <span>{addError.recovery}</span>
          </div>
        )}
        <button
          type="button"
          className="primary-button"
          disabled={picker.path === null || picker.loading || adding}
          {...selectionGuard(() => { if (picker.path !== null) void workbenchStore.addWorkspace(picker.path) })}
        >
          {adding ? '正在添加…' : '添加此文件夹'}
        </button>
      </footer>
    </Dialog>
  )
}
