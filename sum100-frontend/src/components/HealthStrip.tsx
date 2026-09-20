import type { HealthSnapshot } from '../api/types'
import styles from './HealthStrip.module.css'

interface HealthStripProps {
  health: HealthSnapshot
  compact?: boolean
}

export function HealthStrip({ health, compact = false }: HealthStripProps) {
  const metrics = [
    {
      label: 'Feed',
      value: health.connected ? 'Connected' : 'Disconnected',
      connection: true,
    },
    { label: 'Messages received', value: health.messagesReceived.toLocaleString() },
    { label: 'Sequence gaps', value: health.gapCount.toLocaleString() },
    { label: 'Latency p50', value: `${health.latencyP50.toFixed(1)} ms` },
    { label: 'Latency p99', value: `${health.latencyP99.toFixed(1)} ms` },
  ]

  return (
    <div className={`${styles.strip} ${compact ? styles.compact : ''}`}>
      {metrics.map((metric) => (
        <div className={styles.metric} key={metric.label}>
          <span className={styles.label}>{metric.label}</span>
          <span className={styles.value}>
            {metric.connection ? (
              <span
                className={`${styles.dot} ${health.connected ? styles.connected : ''}`}
                aria-hidden="true"
              />
            ) : null}
            {metric.value}
          </span>
        </div>
      ))}
    </div>
  )
}
