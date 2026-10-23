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

// CSS 的折叠动效和 JS 的高度动画共用 motion.ts 的时长：在这里写入 documentElement 的
// CSS 变量，样式表通过同名变量读取。
for (const [property, value] of Object.entries(disclosureCssVariables)) {
  document.documentElement.style.setProperty(property, value)
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
