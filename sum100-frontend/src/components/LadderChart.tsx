import type { LadderPoint } from '../api/types'
import styles from './LadderChart.module.css'

interface LadderChartProps {
  strikes: LadderPoint[]
}

const WIDTH = 560
const HEIGHT = 240
const LEFT = 42
const RIGHT = 18
const TOP = 18
const BOTTOM = 42
const plotWidth = WIDTH - LEFT - RIGHT
const plotHeight = HEIGHT - TOP - BOTTOM

function formatStrike(strike: number) {
  if (Math.abs(strike) >= 1_000) {
    return `$${Math.round(strike / 1_000)}k`
  }

  return String(strike)
}

export function LadderChart({ strikes }: LadderChartProps) {
  const getX = (index: number) =>
    LEFT + (strikes.length === 1 ? plotWidth / 2 : (index / (strikes.length - 1)) * plotWidth)
  const getY = (price: number) => TOP + ((100 - price) / 100) * plotHeight

  return (
    <svg
      className={styles.chart}
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      role="img"
      aria-label="Contract price by strike"
    >
      {[0, 20, 40, 60, 80, 100].map((tick) => (
        <g key={tick}>
          <line
            className={styles.guide}
            x1={LEFT}
            x2={WIDTH - RIGHT}
            y1={getY(tick)}
            y2={getY(tick)}
          />
          <text
            className={styles.tickLabel}
            x={LEFT - 8}
            y={getY(tick) + 3}
            textAnchor="end"
          >
            {tick}¢
          </text>
        </g>
      ))}

      {strikes.slice(1).map((point, index) => {
        const previous = strikes[index]
        const violation = point.violated || previous.violated
        return (
          <line
            className={`${styles.line} ${violation ? styles.violationLine : ''}`}
            key={`${previous.strike}-${point.strike}`}
            x1={getX(index)}
            y1={getY(previous.price)}
            x2={getX(index + 1)}
            y2={getY(point.price)}
          />
        )
      })}

      {strikes.map((point, index) => (
        <g key={point.strike}>
          <circle
            className={`${styles.point} ${point.violated ? styles.violated : ''}`}
            cx={getX(index)}
            cy={getY(point.price)}
            r="4"
          />
          <text
            className={styles.value}
            x={getX(index)}
            y={getY(point.price) - 10}
            textAnchor="middle"
          >
            {point.price}¢
          </text>
          <text
            className={styles.tickLabel}
            x={getX(index)}
            y={HEIGHT - 20}
            textAnchor="middle"
          >
            {formatStrike(point.strike)}
          </text>
        </g>
      ))}

      <text
        className={styles.axisLabel}
        x={LEFT + plotWidth / 2}
        y={HEIGHT - 2}
        textAnchor="middle"
      >
        Strike
      </text>
      <text
        className={styles.axisLabel}
        x="9"
        y={TOP + plotHeight / 2}
        textAnchor="middle"
        transform={`rotate(-90 9 ${TOP + plotHeight / 2})`}
      >
        Price cents
      </text>
    </svg>
  )
}
