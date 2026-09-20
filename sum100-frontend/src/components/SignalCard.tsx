import type { Opportunity } from '../api/types'
import styles from './SignalCard.module.css'

interface SignalCardProps {
  signal: Opportunity
}

export function SignalCard({ signal }: SignalCardProps) {
  return (
    <article className={styles.card}>
      <header className={styles.header}>
        <div>
          <h3 className={styles.pair}>{signal.pair}</h3>
          <div className={styles.event}>{signal.event}</div>
        </div>
        <span className={`${styles.status} ${styles[signal.status]}`}>
          <span className={styles.dot} aria-hidden="true" />
          {signal.status}
        </span>
      </header>

      <div className={styles.tableWrap}>
        <table className={styles.legs}>
          <thead>
            <tr>
              <th>Contract</th>
              <th>Venue</th>
              <th>Action</th>
              <th>Price</th>
              <th>Size</th>
              <th>Fee</th>
            </tr>
          </thead>
          <tbody>
            {signal.legs.map((leg) => (
              <tr key={`${signal.id}-${leg.contract}`}>
                <td className={styles.contract}>{leg.contract}</td>
                <td>{leg.venue}</td>
                <td className={leg.action === 'buy' ? styles.buy : styles.sell}>
                  {leg.action === 'buy' ? '↑ buy' : '↓ sell'}
                </td>
                <td>{leg.price}¢</td>
                <td>{leg.size}</td>
                <td>{leg.fee}¢</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <footer className={styles.footer}>
        <span className={styles.reason}>
          {signal.reason ?? `Detected ${signal.detectedAt}`}
        </span>
        <div className={styles.metricGroup}>
          <span className={styles.metric}>
            <span className={styles.metricLabel}>Edge</span>
            <strong>+{signal.edge}¢</strong>
          </span>
          <span className={styles.metric}>
            <span className={styles.metricLabel}>Ann. return</span>
            <strong>{signal.annualizedReturn.toFixed(1)}%</strong>
          </span>
        </div>
      </footer>
    </article>
  )
}
