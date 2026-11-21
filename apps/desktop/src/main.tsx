import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './app'
import { ErrorBoundary } from './components/ErrorBoundary'
import { applyCssSprings } from './motion'
import { APP_NAME } from './identity'
import { loadPersisted } from './viewPersistence'
import './styles/tokens.css'
import './styles/app.css'

const root = document.getElementById('root')
if (root === null) {
  throw new Error(`${APP_NAME} app root is missing`)
}

document.documentElement.dataset.theme = loadPersisted().theme
applyCssSprings()

createRoot(root).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>,
)
