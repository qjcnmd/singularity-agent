import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './app'
import './styles/tokens.css'
import './styles/app.css'

const root = document.getElementById('root')
if (root === null) {
  throw new Error('Singularity workbench root is missing')
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
