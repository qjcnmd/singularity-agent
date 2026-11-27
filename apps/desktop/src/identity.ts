export const APP_NAME = 'Singularity'
export const APP_SCHEME = 'singularity'
export const APP_ENTRY = `${APP_SCHEME}://app/`
export const HOME_ENV_VAR = 'SINGULARITY_HOME'
export const DATA_DIR_NAME = '.singularity'
export const BACKEND_BINARY = 'singularity.exe'
export const DRAFTS_DB_NAME = 'singularity.drafts'
export const VIEW_STORAGE_KEY = 'singularity.app.view.v1'

export const IPC_CHANNELS = {
  rpc: 'desktop:rpc',
  theme: 'desktop:theme',
  connect: 'desktop:connect',
  frame: 'desktop:frame',
} as const
