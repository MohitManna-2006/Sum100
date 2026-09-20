import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import type { HealthSnapshot, LatencyTick } from '../api/types'
import { HealthView } from './HealthView'

const health: HealthSnapshot = {
  connected: true,
  messagesReceived: 1_000,
  parseErrors: 0,
  gapCount: 0,
  latencyP50: 2,
  latencyP95: 4,
  latencyP99: 6,
  reconnections: 0,
}

function latencyBatch(offset: number): LatencyTick[] {
  return Array.from({ length: 10 }, (_, index) => ({
    label: '00.000',
    value: offset + index,
  }))
}

describe('HealthView', () => {
  it('keeps the rolling latency chart bounded to ten bars', () => {
    const view = render(
      <HealthView
        health={health}
        latency={latencyBatch(0)}
        gaps={[]}
        rejections={[]}
      />,
    )

    for (let update = 1; update <= 25; update += 1) {
      view.rerender(
        <HealthView
          health={health}
          latency={latencyBatch(update)}
          gaps={[]}
          rejections={[]}
        />,
      )
    }

    const chart = screen.getByRole('img', {
      name: 'Ingest latency over the last 10 stream updates',
    })
    expect(chart.children).toHaveLength(10)
  })
})
