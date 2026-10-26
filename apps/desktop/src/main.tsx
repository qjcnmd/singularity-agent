import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { initFluid } from './components/orb/fluid'
import { App } from './app'
import { applyCssSprings } from './motion'
import './styles/tokens.css'
import './styles/app.css'

const root = document.getElementById('root')
if (root === null) {
  throw new Error('Singularity app root is missing')
}

applyCssSprings()

void initFluid().then(() => createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
))
