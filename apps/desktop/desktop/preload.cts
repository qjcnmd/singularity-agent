import { contextBridge, ipcRenderer, webUtils } from 'electron'
import type { StreamEnvelope } from '../src/protocol.generated.js'

contextBridge.exposeInMainWorld('singularity', {
  rpc: (request: unknown) => ipcRenderer.invoke('singularity:rpc', request),
  // 选中或拖入的本地文件的绝对路径；粘贴得到的内存文件返回空字符串。
  pathForFile: (file: File) => webUtils.getPathForFile(file),
  connect: () => ipcRenderer.invoke('singularity:connect'),
  onFrame: (listener: (frame: StreamEnvelope) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, frame: StreamEnvelope) => listener(frame)
    ipcRenderer.on('singularity:frame', handler)
    return () => ipcRenderer.removeListener('singularity:frame', handler)
  },
  onFailure: (listener: () => void) => {
    const handler = () => listener()
    ipcRenderer.on('singularity:failure', handler)
    return () => ipcRenderer.removeListener('singularity:failure', handler)
  },
})
