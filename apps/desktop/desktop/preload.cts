import { contextBridge, ipcRenderer, webUtils } from 'electron'
import type { StreamEnvelope } from '../src/protocol.generated.js'

contextBridge.exposeInMainWorld('desktop', {
  rpc: (request: unknown) => ipcRenderer.invoke('desktop:rpc', request),
  setTheme: (theme: 'light' | 'dark') => ipcRenderer.invoke('desktop:theme', theme),
  // 选中或拖入的本地文件的绝对路径；粘贴得到的内存文件返回空字符串。
  pathForFile: (file: File) => webUtils.getPathForFile(file),
  connect: () => ipcRenderer.invoke('desktop:connect'),
  onFrame: (listener: (frame: StreamEnvelope) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, frame: StreamEnvelope) => listener(frame)
    ipcRenderer.on('desktop:frame', handler)
    return () => ipcRenderer.removeListener('desktop:frame', handler)
  },
  onFailure: (listener: () => void) => {
    const handler = () => listener()
    ipcRenderer.on('desktop:failure', handler)
    return () => ipcRenderer.removeListener('desktop:failure', handler)
  },
})
