import { app, BrowserWindow, dialog, ipcMain, Menu, nativeImage, net, protocol, shell, Tray } from 'electron'
import { join, resolve, relative, isAbsolute } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { homedir } from 'node:os'
import { createHash } from 'node:crypto'
import { Backend } from './backend.js'
import type { DirectoryPickResult, RpcResponse } from '../src/protocol.generated.js'

const entry = 'singularity://app/'
const home = resolve(process.env.SINGULARITY_HOME || join(homedir(), '.singularity'))
// 不同数据目录不得共用 Electron 的 Chromium profile 锁和视图状态。
app.setPath('userData', join(app.getPath('appData'), 'Singularity', createHash('sha256').update(home.toLowerCase()).digest('hex').slice(0, 16)))
protocol.registerSchemesAsPrivileged([{ scheme: 'singularity', privileges: { standard: true, secure: true, supportFetchAPI: true } }])
let window: BrowserWindow
let tray: Tray
let backend: Backend
let subscribed = false
let quitting = false
let stopping = false
let picker: Promise<Electron.OpenDialogReturnValue> | null = null

function show(): void {
  if (!window || window.isDestroyed()) return
  if (window.isMinimized()) window.restore()
  window.show()
  window.focus()
}

function trusted(event: Electron.IpcMainInvokeEvent): void {
  if (event.sender !== window.webContents || event.senderFrame !== window.webContents.mainFrame || event.senderFrame.url !== entry) {
    throw new Error('Untrusted desktop IPC sender')
  }
}

async function start(): Promise<void> {
  const assets = join(app.getAppPath(), 'dist')
  const icon = nativeImage.createFromPath(join(app.getAppPath(), 'resources', 'icon.png'))
  protocol.handle('singularity', request => {
    const url = new URL(request.url)
    const file = resolve(assets, '.' + decodeURIComponent(url.pathname === '/' ? '/index.html' : url.pathname))
    const path = relative(assets, file)
    if (url.host !== 'app' || path.startsWith('..') || isAbsolute(path)) return new Response('Not found', { status: 404 })
    return net.fetch(pathToFileURL(file).href)
  })
  backend = new Backend(app.isPackaged ? join(process.resourcesPath, 'runtime', 'singularity.exe') : join(app.getAppPath(), 'desktop-runtime', 'singularity.exe'))
  backend.on('failure', (error: Error) => {
    // 窗口还没建好时，start() 只报告一次被拒绝的 ready promise。
    if (!window || window.isDestroyed()) return
    window.webContents.send('singularity:failure')
    void dialog.showMessageBox({ type: 'error', title: 'Singularity', message: '工作台后端已停止', detail: error.message, buttons: ['退出'] }).then(() => app.quit())
  })
  await backend.ready
  window = new BrowserWindow({
    title: 'Singularity', width: 1440, height: 960, minWidth: 720, minHeight: 480,
    show: false, backgroundColor: '#f2f2f7', icon,
    webPreferences: { preload: join(fileURLToPath(new URL('.', import.meta.url)), 'preload.cjs'), contextIsolation: true, sandbox: true, nodeIntegration: false },
  })
  Menu.setApplicationMenu(Menu.buildFromTemplate([{ label: 'Singularity', submenu: [
    { label: '刷新工作台', accelerator: 'CmdOrCtrl+R', click: () => window.reload() },
    { role: 'toggleDevTools' }, { type: 'separator' }, { label: '退出', click: () => app.quit() },
  ] }, { role: 'editMenu' }]))
  window.setMenuBarVisibility(false)
  window.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:\/\//.test(url)) void shell.openExternal(url)
    return { action: 'deny' }
  })
  window.webContents.on('will-navigate', (event, url) => { if (url !== entry) event.preventDefault() })
  window.webContents.on('did-start-loading', () => { subscribed = false })
  window.webContents.session.setPermissionRequestHandler((contents, permission, callback) => {
    callback(contents === window.webContents && contents.getURL() === entry && permission === 'clipboard-sanitized-write')
  })
  backend.on('frame', frame => { if (subscribed && !window.isDestroyed()) window.webContents.send('singularity:frame', frame) })
  ipcMain.handle('singularity:connect', async event => {
    trusted(event)
    const frame = await backend.connect()
    subscribed = true
    return frame
  })
  ipcMain.handle('singularity:rpc', async (event, request): Promise<RpcResponse> => {
    trusted(event)
    if (request?.method !== 'directory.pick') return backend.rpc(request)
    try {
      show()
      picker ??= dialog.showOpenDialog(window, { title: '选择工作区文件夹', properties: ['openDirectory'] }).finally(() => { picker = null })
      const result = await picker
      const selected: DirectoryPickResult = { path: result.canceled ? null : result.filePaths[0] }
      return { type: 'success', result: selected }
    } catch (error) {
      return { type: 'error', error: { code: 'internal', message: `无法选择文件夹：${String(error)}`, recovery: '请重试添加工作区。' } }
    }
  })
  tray = new Tray(icon.resize({ width: 20, height: 20 }))
  tray.setToolTip('Singularity')
  tray.setContextMenu(Menu.buildFromTemplate([{ label: '打开 Singularity', click: show }, { label: '退出', click: () => app.quit() }]))
  tray.on('click', show)
  window.on('close', event => { if (!quitting) { event.preventDefault(); window.hide() } })
  await window.loadURL(entry)
  show()
}

if (!app.requestSingleInstanceLock()) app.quit()
else {
  app.on('second-instance', show)
  app.on('before-quit', event => {
    if (quitting) return
    event.preventDefault()
    if (stopping) return
    stopping = true
    void (async () => {
      await backend?.stop()
      quitting = true
      tray?.destroy()
      app.quit()
    })()
  })
  void app.whenReady().then(start).catch(error => { dialog.showErrorBox('Singularity 启动失败', String(error)); app.quit() })
}
