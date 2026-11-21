import { Component, type ErrorInfo, type ReactNode } from 'react'
import { APP_NAME } from '../identity'

interface ErrorBoundaryState {
  error: Error | null
}

export class ErrorBoundary extends Component<{ children: ReactNode }, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error(`${APP_NAME} UI error:`, error, info.componentStack)
  }

  render() {
    if (!this.state.error) return this.props.children
    return (
      <div className="error-boundary" role="alert">
        <div className="error-boundary-card">
          <h1>工作台界面发生异常</h1>
          <p>{this.state.error.message || '发生了未预期的渲染错误。'}</p>
          <button type="button" className="primary-button" onClick={() => window.location.reload()}>
            重新加载界面
          </button>
        </div>
      </div>
    )
  }
}
