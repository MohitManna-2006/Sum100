import type { ConnectionPhase, HealthSnapshot } from '../api/types'
import { HealthStrip } from './HealthStrip'
import styles from './Header.module.css'

interface HeaderProps {
  health: HealthSnapshot
  phase: ConnectionPhase
  lastUpdate?: number
  onReconnect: () => void
  onStop: () => void
  canStop: boolean
  controlPending?: 'starting' | 'stopping' | null
}

export function Header({
  health,
  phase,
  lastUpdate,
  onReconnect,
  onStop,
  canStop,
  controlPending = null,
}: HeaderProps) {
  const lastUpdateLabel = lastUpdate
    ? new Date(lastUpdate).toLocaleTimeString([], {
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
      })
    : 'Waiting'

  return (
    <header className={styles.header}>
      <div className={styles.brand}>
        <span className={styles.mark} aria-hidden="true">
          Σ
        </span>
        <h1 className={styles.wordmark}>
          Sum100 <span className={styles.descriptor}>Market coherence</span>
        </h1>
      </div>
      <div className={styles.status}>
        <div className={styles.connectionMeta}>
          <span className={`${styles.phase} ${styles[phase]}`}>{phase}</span>
          <span className={styles.lastUpdate}>Update {lastUpdateLabel}</span>
          {phase !== 'connected' ? (
            <button
              className={styles.reconnect}
              type="button"
              onClick={onReconnect}
              disabled={controlPending !== null}
            >
              {controlPending === 'starting' ? 'Starting…' : 'Reconnect'}
            </button>
          ) : canStop ? (
            <button
              className={styles.reconnect}
              type="button"
              onClick={onStop}
              disabled={controlPending !== null}
            >
              {controlPending === 'stopping' ? 'Stopping…' : 'Stop engine'}
            </button>
          ) : null}
        </div>
        <HealthStrip health={health} compact />
      </div>
    </header>
  )
}
