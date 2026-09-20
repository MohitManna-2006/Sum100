import type {
  GapTick,
  HealthSnapshot,
  LatencyTick,
  RejectionMetric,
} from '../api/types'
import { HealthStrip } from '../components/HealthStrip'
import styles from './HealthView.module.css'

interface HealthViewProps {
  health: HealthSnapshot
  latency: LatencyTick[]
  gaps: GapTick[]
  rejections: RejectionMetric[]
}

const GAP_WIDTH = 520
const GAP_HEIGHT = 220
const GAP_LEFT = 30
const GAP_RIGHT = 14
const GAP_TOP = 18
const GAP_BOTTOM = 32

export function HealthView({ health, latency, gaps, rejections }: HealthViewProps) {
  const visibleLatency = latency.slice(-10)
  const visibleGaps = gaps.slice(-10)
  const totalRejections = rejections.reduce((total, metric) => total + metric.count, 0)
  const latencyCeiling = Math.max(
    1,
    health.latencyP99,
    ...visibleLatency.map((tick) => tick.value),
  )
  const gapCeiling = Math.max(3, ...visibleGaps.map((tick) => tick.count))
  const gapTicks = [0, 1, 2, 3].map((step) =>
    Math.round((gapCeiling / 3) * step),
  )
  const gapPlotWidth = GAP_WIDTH - GAP_LEFT - GAP_RIGHT
  const gapPlotHeight = GAP_HEIGHT - GAP_TOP - GAP_BOTTOM
  const gapX = (index: number) =>
    GAP_LEFT +
    (visibleGaps.length === 1
      ? 0
      : (index / (visibleGaps.length - 1)) * gapPlotWidth)
  const gapY = (count: number) =>
    GAP_TOP + ((gapCeiling - count) / gapCeiling) * gapPlotHeight
  const gapPoints = visibleGaps
    .map((tick, index) => `${gapX(index)},${gapY(tick.count)}`)
    .join(' ')

  return (
    <section className={styles.view}>
      <div className={styles.topline}>
        <div>
          <h2 className={styles.title}>Engine health</h2>
          <p className={styles.subtitle}>
            Feed throughput, latency, sequence continuity, and signal disposition.
          </p>
        </div>
        <span className={styles.uptime}>
          {health.messagesReceived.toLocaleString()} messages · {health.reconnections}{' '}
          reconnects · {health.parseErrors} parse errors
        </span>
      </div>

      <HealthStrip health={health} />

      <article className={`${styles.panel} ${styles.widePanel}`}>
        <header className={styles.panelHeader}>
          <div>
            <h3 className={styles.panelTitle}>Venues</h3>
            <span className={styles.panelNote}>
              Each feed is its own socket, sequence, and verdict
            </span>
          </div>
          <span className={styles.panelValue}>{health.overall}</span>
        </header>
        <div className={styles.rejectionList}>
          {health.venues.map((venue) => (
            <div className={styles.rejectionItem} key={venue.venue}>
              <span className={styles.rejectionLabel}>
                <span
                  className={`${styles.legendDot} ${
                    venue.connected ? styles.gain : venue.subscribed ? styles.loss : styles.info
                  }`}
                  aria-hidden="true"
                />
                {venue.label}
              </span>
              <strong className={styles.rejectionValue}>
                {venue.subscribed
                  ? `${venue.state} · ${venue.messagesReceived.toLocaleString()} msg · ${venue.parseErrors} parse errors · ${venue.reconnections} reconnects · p99 ${venue.latencyP99.toFixed(1)} ms`
                  : 'not subscribed'}
              </strong>
            </div>
          ))}
        </div>
      </article>

      <div className={styles.metricGrid}>
        <article className={styles.panel}>
          <header className={styles.panelHeader}>
            <div>
              <h3 className={styles.panelTitle}>Ingest latency</h3>
              <span className={styles.panelNote}>Latest 10 updates · ms</span>
            </div>
            <span className={styles.panelValue}>
              p95 {health.latencyP95.toFixed(1)} · p99 {health.latencyP99.toFixed(1)} ms
            </span>
          </header>
          <div
            className={styles.latencyChart}
            role="img"
            aria-label="Ingest latency over the last 10 stream updates"
          >
            {visibleLatency.map((tick, index) => (
              <div
                className={styles.latencyItem}
                key={index}
                title={`${tick.label}: ${tick.value.toFixed(1)} ms`}
              >
                <div className={styles.barTrack}>
                  <div
                    className={styles.bar}
                    style={{
                      height: `${Math.min(
                        100,
                        Math.max(0, (tick.value / latencyCeiling) * 100),
                      )}%`,
                    }}
                  >
                    <span className={styles.barValue}>{tick.value.toFixed(1)}</span>
                  </div>
                </div>
                <span className={styles.tick}>{tick.label}</span>
              </div>
            ))}
          </div>
        </article>

        <article className={styles.panel}>
          <header className={styles.panelHeader}>
            <div>
              <h3 className={styles.panelTitle}>Sequence gaps</h3>
              <span className={styles.panelNote}>Cumulative count · last 10 updates</span>
            </div>
            <span className={styles.panelValue}>{health.gapCount} current</span>
          </header>
          <svg
            className={styles.gapChart}
            viewBox={`0 0 ${GAP_WIDTH} ${GAP_HEIGHT}`}
            role="img"
            aria-label="Sequence gap count by stream update"
          >
            {gapTicks.map((tick, index) => (
              <g key={`${tick}-${index}`}>
                <line
                  className={styles.gapGuide}
                  x1={GAP_LEFT}
                  x2={GAP_WIDTH - GAP_RIGHT}
                  y1={gapY(tick)}
                  y2={gapY(tick)}
                />
                <text
                  className={styles.chartLabel}
                  x={GAP_LEFT - 8}
                  y={gapY(tick) + 3}
                  textAnchor="end"
                >
                  {tick}
                </text>
              </g>
            ))}
            <polyline className={styles.gapLine} points={gapPoints} />
            {visibleGaps.map((tick, index) => (
              <g key={index}>
                <circle
                  className={styles.gapPoint}
                  cx={gapX(index)}
                  cy={gapY(tick.count)}
                  r="3.5"
                />
                {index % 2 === 0 || index === visibleGaps.length - 1 ? (
                  <text
                    className={styles.chartLabel}
                    x={gapX(index)}
                    y={GAP_HEIGHT - 8}
                    textAnchor="middle"
                  >
                    {tick.hour}
                  </text>
                ) : null}
              </g>
            ))}
          </svg>
        </article>

        <article className={`${styles.panel} ${styles.widePanel}`}>
          <header className={styles.panelHeader}>
            <div>
              <h3 className={styles.panelTitle}>Signal disposition</h3>
              <span className={styles.panelNote}>
                Accepted and rejected candidates · latest update
              </span>
            </div>
            <span className={styles.panelValue}>{totalRejections.toLocaleString()} evaluated</span>
          </header>
          <div className={styles.funnelBody}>
            {totalRejections > 0 ? (
              <>
                <div className={styles.funnel} aria-label="Signal disposition proportions">
                  {rejections.map((metric) => (
                    <span
                      className={`${styles.segment} ${styles[metric.tone]}`}
                      key={metric.label}
                      style={{ width: `${(metric.count / totalRejections) * 100}%` }}
                      title={`${metric.label}: ${metric.count}`}
                    />
                  ))}
                </div>
                <div className={styles.rejectionList}>
                  {rejections.map((metric) => (
                    <div className={styles.rejectionItem} key={metric.label}>
                      <span className={styles.rejectionLabel}>
                        <span
                          className={`${styles.legendDot} ${styles[metric.tone]}`}
                          aria-hidden="true"
                        />
                        {metric.label}
                      </span>
                      <strong className={styles.rejectionValue}>
                        {metric.count.toLocaleString()}
                      </strong>
                    </div>
                  ))}
                </div>
              </>
            ) : (
              <div className={styles.emptyFunnel}>No opportunities in the latest update.</div>
            )}
          </div>
        </article>
      </div>
    </section>
  )
}
