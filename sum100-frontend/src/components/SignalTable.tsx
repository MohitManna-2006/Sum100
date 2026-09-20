import type { Opportunity, SortKey } from '../api/types'
import { SignalCard } from './SignalCard'
import styles from './SignalTable.module.css'

interface SignalTableProps {
  opportunities: Opportunity[]
  sort: SortKey
  onSort: (sort: SortKey) => void
  onInspect: (signal: Opportunity) => void
}

const sortHeaders: Array<{ key: SortKey; label: string }> = [
  { key: 'edge', label: 'Edge' },
  { key: 'return', label: 'Return' },
  { key: 'days', label: 'Days' },
]

export function SignalTable({
  opportunities,
  sort,
  onSort,
  onInspect,
}: SignalTableProps) {
  if (opportunities.length === 0) {
    return <div className={styles.empty}>No signals match this filter.</div>
  }

  return (
    <div className={styles.shell}>
      <table className={styles.table}>
        <colgroup>
          <col style={{ width: '31%' }} />
          <col style={{ width: '14%' }} />
          <col style={{ width: '10%' }} />
          <col style={{ width: '12%' }} />
          <col style={{ width: '9%' }} />
          <col style={{ width: '16%' }} />
          <col style={{ width: '8%' }} />
        </colgroup>
        <thead>
          <tr>
            <th>Pair</th>
            <th>Venue</th>
            {sortHeaders.map((header) => (
              <th className={styles.numeric} key={header.key}>
                <button
                  className={`${styles.sortButton} ${
                    sort === header.key ? styles.active : ''
                  }`}
                  type="button"
                  onClick={() => onSort(header.key)}
                  aria-pressed={sort === header.key}
                >
                  {header.label}
                </button>
              </th>
            ))}
            <th>Status</th>
            <th aria-label="Actions" />
          </tr>
        </thead>
        <tbody>
          {opportunities.map((signal) => (
            <tr key={signal.id}>
              <td>
                <span className={styles.pairCell}>
                  <span className={styles.pair}>{signal.pair}</span>
                  <span className={styles.event}>{signal.event}</span>
                </span>
              </td>
              <td className={styles.venue}>
                {signal.legs.map((leg) => leg.venue).join(' / ')}
              </td>
              <td className={`${styles.numeric} ${styles.gain}`}>+{signal.edge}¢</td>
              <td className={`${styles.numeric} ${styles.gain}`}>
                {signal.annualizedReturn.toFixed(1)}%
              </td>
              <td className={styles.numeric}>{signal.daysToResolution}d</td>
              <td>
                <span className={`${styles.status} ${styles[signal.status]}`}>
                  {signal.status}
                </span>
              </td>
              <td className={styles.numeric}>
                <button
                  className={styles.inspect}
                  type="button"
                  onClick={() => onInspect(signal)}
                  aria-label={`Inspect ${signal.pair}`}
                >
                  →
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <div className={styles.mobileCards}>
        {opportunities.map((signal) => (
          <SignalCard key={signal.id} signal={signal} />
        ))}
      </div>
    </div>
  )
}
