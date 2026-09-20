import type { ConnectionPhase } from '../api/types'
import styles from './ConnectionState.module.css'

interface ConnectionStateProps {
  phase: ConnectionPhase
  error: string | null
  onReconnect: () => void
  reconnectPending?: boolean
  banner?: boolean
}

export function ConnectionState({
  phase,
  error,
  onReconnect,
  reconnectPending = false,
  banner = false,
}: ConnectionStateProps) {
  if (banner) {
    return (
      <div className={styles.banner} role="status">
        <span>{error || 'Live stream interrupted; displayed data is stale.'}</span>
        <button
          className={styles.reconnect}
          type="button"
          onClick={onReconnect}
          disabled={reconnectPending}
        >
          {reconnectPending ? 'Starting…' : 'Reconnect'}
        </button>
      </div>
    )
  }

  const waiting = phase === 'connected'
  const title = waiting
    ? 'Waiting for first update'
    : phase === 'offline'
      ? 'Engine stopped'
    : phase === 'reconnecting'
      ? 'Reconnecting to engine'
      : 'Connecting to engine'

  return (
    <div className={styles.state} role="status" aria-live="polite">
      <div className={styles.inner}>
        <span className={styles.spinner} aria-hidden="true" />
        <h2 className={styles.title}>{title}</h2>
        <p className={styles.detail}>
          {error || 'Opening the live state stream on the configured API endpoint.'}
        </p>
        {phase !== 'connected' ? (
          <button
            className={styles.reconnect}
            type="button"
            onClick={onReconnect}
            disabled={reconnectPending}
          >
            {reconnectPending ? 'Starting engine…' : 'Reconnect now'}
          </button>
        ) : null}
      </div>
    </div>
  )
}
