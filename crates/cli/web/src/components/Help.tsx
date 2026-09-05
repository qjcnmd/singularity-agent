import { workbenchStore } from '../store'
import { Dialog } from './Dialog'

export function Help({ open }: { open: boolean }) {
  return (
    <Dialog open={open} onClose={() => workbenchStore.setHelpOpen(false)} labelledBy="help-title" className="help-modal">
      <header className="modal-header">
        <div><span className="eyebrow">使用帮助</span><h2 id="help-title">与 Singularity 协作</h2></div>
        <button type="button" className="icon-button" data-autofocus onClick={() => workbenchStore.setHelpOpen(false)} aria-label="关闭">×</button>
      </header>
      <div className="help-content">
        <article><strong>描述完整目标</strong><p>说明想得到的结果、限制和重要文件。多行内容可以直接粘贴，输入框会原样保留。</p></article>
        <article><strong>运行中调整方向</strong><p>“即时转向”会送入当前回合；“后续消息”会进入有编号的队列，在当前工作结束后按顺序执行。</p></article>
        <article><strong>管理后续消息</strong><p>每条排队消息都能编辑、撤回或立即发送。操作失败时，原内容会留在对应位置供你重试。</p></article>
        <article><strong>理解会话记录</strong><p>消息、思考、命令、文件变更、诊断和终态按来源顺序展示。点“详情”可查看完整参数和输出。</p></article>
        <article><strong>压缩上下文</strong><p>长会话可压缩为持久摘要以释放模型上下文。压缩期间仍能编辑草稿，也可以停止。</p></article>
        <article><strong>完整本机权限</strong><p>Agent 使用当前账户读写本机文件并运行命令。项目用于选择工作上下文，并不限制权限范围。</p></article>
        <article><strong>常用按键</strong><p><kbd>Enter</kbd> 发送，<kbd>Shift+Enter</kbd> 换行，<kbd>@</kbd> 查找项目文件，<kbd>/</kbd> 打开固定命令。</p></article>
      </div>
      <footer className="modal-footer"><p>快速说明可以随时重新显示。</p><button type="button" className="secondary-button" onClick={() => { workbenchStore.setGuideCollapsed(false); workbenchStore.setHelpOpen(false) }}>显示快速说明</button></footer>
    </Dialog>
  )
}
