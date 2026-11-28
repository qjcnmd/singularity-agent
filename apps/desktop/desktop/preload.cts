import electron = require('electron')
import type { IPC_CHANNELS } from '../src/identity.js'
import type { StreamEnvelope } from '../src/protocol.generated.js'

const { contextBridge, ipcRenderer, webUtils } = electron

contextBridge.exposeInMainWorld('desktop', {
  rpc: (request: unknown) => ipcRenderer.invoke('desktop:rpc' satisfies (typeof IPC_CHANNELS)['rpc'], request),
  setTheme: (theme: 'light' | 'dark') => ipcRenderer.invoke('desktop:theme' satisfies (typeof IPC_CHANNELS)['theme'], theme),
  // 选中或拖入的本地文件的绝对路径；粘贴得到的内存文件返回空字符串。
  pathForFile: (file: File) => webUtils.getPathForFile(file),
  connect: () => ipcRenderer.invoke('desktop:connect' satisfies (typeof IPC_CHANNELS)['connect']),
  onFrame: (listener: (frame: StreamEnvelope) => void) => {
    ipcRenderer.on('desktop:frame' satisfies (typeof IPC_CHANNELS)['frame'], (_event: Electron.IpcRendererEvent, frame: StreamEnvelope) => listener(frame))
  },
})
