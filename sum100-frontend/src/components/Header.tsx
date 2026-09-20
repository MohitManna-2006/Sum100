import type { HealthSnapshot } from '../api/types'
import { HealthStrip } from './HealthStrip'
import styles from './Header.module.css'

interface HeaderProps {
  health: HealthSnapshot
}

export function Header({ health }: HeaderProps) {
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
        <HealthStrip health={health} compact />
      </div>
    </header>
  )
}
