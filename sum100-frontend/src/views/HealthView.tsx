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
  const totalRejections = rejections.reduce((total, metric) => total + metric.count, 0)
  const gapPlotWidth = GAP_WIDTH - GAP_LEFT - GAP_RIGHT
  const gapPlotHeight = GAP_HEIGHT - GAP_TOP - GAP_BOTTOM
  const gapX = (index: number) =>
    GAP_LEFT + (gaps.length === 1 ? 0 : (index / (gaps.length - 1)) * gapPlotWidth)
  const gapY = (count: number) => GAP_TOP + ((3 - count) / 3) * gapPlotHeight
  const gapPoints = gaps.map((tick, index) => `${gapX(index)},${gapY(tick.count)}`).join(' ')

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
          Uptime {health.uptime} · {health.contractsTracked.toLocaleString()} contracts
        </span>
      </div>

      <HealthStrip health={health} />

      <div className={styles.metricGrid}>
        <article className={styles.panel}>
          <header className={styles.panelHeader}>
            <div>
              <h3 className={styles.panelTitle}>Ingest latency</h3>
              <span className={styles.panelNote}>Last 10 engine ticks · milliseconds</span>
            </div>
            <span className={styles.panelValue}>p99 {health.latencyP99.toFixed(1)} ms</span>
          </header>
          <div className={styles.latencyChart}>
            {latency.map((tick) => (
              <div className={styles.latencyItem} key={tick.label}>
                <div className={styles.barTrack}>
                  <div
                    className={styles.bar}
                    style={{ height: `${Math.min(100, tick.value * 10)}%` }}
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
              <span className={styles.panelNote}>Observed per hour · last 6 hours</span>
            </div>
            <span className={styles.panelValue}>{health.gapCount} current</span>
          </header>
          <svg
            className={styles.gapChart}
            viewBox={`0 0 ${GAP_WIDTH} ${GAP_HEIGHT}`}
            role="img"
            aria-label="Sequence gaps observed by hour"
          >
            {[0, 1, 2, 3].map((tick) => (
              <g key={tick}>
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
            {gaps.map((tick, index) => (
              <g key={tick.hour}>
                <circle
                  className={styles.gapPoint}
                  cx={gapX(index)}
                  cy={gapY(tick.count)}
                  r="3.5"
                />
                <text
                  className={styles.chartLabel}
                  x={gapX(index)}
                  y={GAP_HEIGHT - 8}
                  textAnchor="middle"
                >
                  {tick.hour}
                </text>
              </g>
            ))}
          </svg>
        </article>

        <article className={`${styles.panel} ${styles.widePanel}`}>
          <header className={styles.panelHeader}>
            <div>
              <h3 className={styles.panelTitle}>Signal disposition</h3>
              <span className={styles.panelNote}>
                Accepted and rejected candidates · rolling hour
              </span>
            </div>
            <span className={styles.panelValue}>{totalRejections.toLocaleString()} evaluated</span>
          </header>
          <div className={styles.funnelBody}>
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
          </div>
        </article>
      </div>
    </section>
  )
}
