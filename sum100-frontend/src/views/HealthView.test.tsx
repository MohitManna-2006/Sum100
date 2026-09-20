import { render, screen, within } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import type { HealthSnapshot, LatencyTick } from '../api/types'
import { HealthView } from './HealthView'

const health: HealthSnapshot = {
  connected: true,
  overall: 'connected',
  venues: [
    {
      venue: 'kalshi',
      label: 'Kalshi',
      connected: true,
      subscribed: true,
      state: 'healthy',
      messagesReceived: 1_000,
      parseErrors: 0,
      reconnections: 0,
      latencyP50: 2,
      latencyP99: 6,
    },
    {
      venue: 'polymarket',
      label: 'Polymarket',
      connected: false,
      subscribed: false,
      state: 'disconnected',
      messagesReceived: 0,
      parseErrors: 0,
      reconnections: 0,
      latencyP50: 0,
      latencyP99: 0,
    },
  ],
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

  it('names every venue and marks the one nothing feeds as absent', () => {
    // Scoped to this render: the suite has no cleanup hook, so earlier tests
    // leave their markup in the document.
    const { container } = render(
      <HealthView
        health={health}
        latency={latencyBatch(0)}
        gaps={[]}
        rejections={[]}
      />,
    )
    const panel = within(container)

    expect(panel.getByText('Kalshi')).toBeDefined()
    expect(panel.getByText('Polymarket')).toBeDefined()
    // An unsubscribed venue reads as absent, never as a feed that is down.
    expect(panel.getByText('not subscribed')).toBeDefined()
    expect(panel.getByText(/1,000 msg/)).toBeDefined()
  })
})
