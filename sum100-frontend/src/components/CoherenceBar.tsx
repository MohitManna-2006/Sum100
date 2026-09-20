import type { CoherenceEvent } from '../api/types'
import styles from './CoherenceBar.module.css'

interface CoherenceBarProps {
  event: CoherenceEvent
  onExpand?: () => void
}

export function CoherenceBar({ event, onExpand }: CoherenceBarProps) {
  return (
    <div className={styles.shell}>
      <div className={styles.labels} aria-hidden="true">
        {event.outcomes.map((outcome) => (
          <span
            className={styles.label}
            key={outcome.label}
            style={{ flexBasis: `${outcome.price}%` }}
          >
            <span>{outcome.label} </span>
            <strong>{outcome.price}¢</strong>
          </span>
        ))}
      </div>

      <button
        className={styles.barButton}
        type="button"
        onClick={onExpand}
        aria-label={`Expand ${event.name} outcome breakdown`}
      >
        {event.outcomes.map((outcome) => (
          <span
            className={styles.segment}
            key={outcome.label}
            style={{ flexBasis: `${outcome.price}%` }}
            title={`${outcome.label}: ${outcome.price}¢${
              outcome.fee ? `, ${outcome.fee}¢ fee` : ''
            }`}
          >
            {outcome.price >= 12 ? `${outcome.price}¢` : ''}
            {outcome.fee ? (
              <span
                className={styles.fee}
                style={{ width: `${(outcome.fee / outcome.price) * 100}%` }}
              />
            ) : null}
          </span>
        ))}
        {event.totalPrice < event.target ? (
          <span
            className={styles.remainder}
            style={{ flexBasis: `${event.target - event.totalPrice}%` }}
            aria-hidden="true"
          />
        ) : null}
      </button>

      <div className={styles.targetLine}>
        <span className={styles.target}>{event.target}¢ target</span>
        <span className={`${styles.gap} ${event.gap === 0 ? styles.zeroGap : ''}`}>
          {event.gap === 0 ? 'At target' : `${event.gap}¢ gap`}
        </span>
      </div>
    </div>
  )
}
