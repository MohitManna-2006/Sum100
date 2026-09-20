import { useState } from 'react'
import type { CoherenceEvent } from '../api/types'
import { CoherenceBar } from '../components/CoherenceBar'
import { LadderChart } from '../components/LadderChart'
import { Modal } from '../components/Modal'
import styles from './CoherenceView.module.css'

interface CoherenceViewProps {
  events: CoherenceEvent[]
}

export function CoherenceView({ events }: CoherenceViewProps) {
  const [selectedId, setSelectedId] = useState(events[0]?.id ?? '')
  const [detailsOpen, setDetailsOpen] = useState(false)
  const selected = events.find((event) => event.id === selectedId) ?? events[0]

  if (!selected) {
    return null
  }

  return (
    <section className={styles.view}>
      <div className={styles.topline}>
        <div>
          <h2 className={styles.title}>Outcome coherence</h2>
          <p className={styles.subtitle}>
            Preview data · awaiting the backend coherence state contract.
          </p>
        </div>
        <select
          className={styles.selector}
          value={selected.id}
          onChange={(event) => setSelectedId(event.target.value)}
          aria-label="Select event"
        >
          {events.map((event) => (
            <option value={event.id} key={event.id}>
              {event.id}
            </option>
          ))}
        </select>
      </div>

      <article className={styles.panel}>
        <header className={styles.panelHeader}>
          <div>
            <h3 className={styles.eventName}>{selected.name}</h3>
            <span className={styles.eventMeta}>
              {selected.id} · {selected.venue} · {selected.outcomes.length} outcomes
            </span>
          </div>
          <span className={`${styles.status} ${styles[selected.status]}`}>
            {selected.status}
          </span>
        </header>

        <div className={styles.barWrap}>
          <CoherenceBar event={selected} onExpand={() => setDetailsOpen(true)} />
        </div>

        <div className={styles.summaryLine}>
          <span className={styles.summaryText}>{selected.summary}</span>
          <span className={styles.expandHint}>Select bar for detail →</span>
        </div>
      </article>

      <div className={styles.lowerGrid}>
        <article className={styles.chartPanel}>
          <header className={styles.sectionHeader}>
            <h3 className={styles.sectionTitle}>Strike ladder</h3>
            <div className={styles.sectionNote}>
              Red marks a monotonicity inversion in the displayed book.
            </div>
          </header>
          <div className={styles.chartBody}>
            <LadderChart strikes={selected.strikes} />
          </div>
        </article>

        <aside className={styles.metricsPanel}>
          <header className={styles.sectionHeader}>
            <h3 className={styles.sectionTitle}>Group metrics</h3>
            <div className={styles.sectionNote}>Executable displayed prices</div>
          </header>
          <dl className={styles.metrics}>
            <div className={styles.metric}>
              <dt>Outcome sum</dt>
              <dd>{selected.totalPrice}¢</dd>
            </div>
            <div className={styles.metric}>
              <dt>Target</dt>
              <dd>{selected.target}¢</dd>
            </div>
            <div className={styles.metric}>
              <dt>Gross gap</dt>
              <dd className={selected.gap > 0 ? styles.gain : ''}>{selected.gap}¢</dd>
            </div>
            <div className={styles.metric}>
              <dt>Fees at depth</dt>
              <dd className={styles.loss}>−{selected.totalFees}¢</dd>
            </div>
            <div className={styles.metric}>
              <dt>Net edge</dt>
              <dd className={selected.gap > selected.totalFees ? styles.gain : styles.loss}>
                {selected.gap - selected.totalFees}¢
              </dd>
            </div>
          </dl>
        </aside>
      </div>

      {detailsOpen ? (
        <Modal title={`${selected.id} outcome detail`} onClose={() => setDetailsOpen(false)}>
          <table className={styles.outcomeTable}>
            <thead>
              <tr>
                <th>Outcome</th>
                <th>Ask</th>
                <th>Midpoint</th>
                <th>Fee</th>
              </tr>
            </thead>
            <tbody>
              {selected.outcomes.map((outcome) => (
                <tr key={outcome.label}>
                  <td>{outcome.label}</td>
                  <td>{outcome.price}¢</td>
                  <td>{outcome.midpoint?.toFixed(1) ?? '—'}¢</td>
                  <td>{outcome.fee ?? 0}¢</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Modal>
      ) : null}
    </section>
  )
}
