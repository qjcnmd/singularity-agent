import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './app'
import { disclosureCssVariables } from './motion'
import './styles/tokens.css'
import './styles/app.css'

const root = document.getElementById('root')
if (root === null) {
  throw new Error('Singularity app root is missing')
}

// CSS 的 disclosure 与 JS 高度动画共用 motion.ts 的时序：数值在这里派生一次，
// 写入 documentElement 后由同名变量消费。
for (const [property, value] of Object.entries(disclosureCssVariables)) {
  document.documentElement.style.setProperty(property, value)
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
