import type { EngineState } from '../api/client'

export function engineState(timestamp = 1_789_000_000_000): EngineState {
  return {
    timestamp_ms: timestamp,
    opportunities: [
      {
        id: 'opp-live-1',
        group_id: 'fed-september',
        relation: 'exhaustive',
        members: ['fed-cut', 'fed-hold'],
        legs: [
          {
            contract_id: 'fed-cut',
            venue: 'kalshi',
            side: 'yes',
            action: 'buy',
            best_ask: 48,
            best_bid: 47,
            size: 100,
            fee: 1,
          },
          {
            contract_id: 'fed-hold',
            venue: 'polymarket',
            side: 'yes',
            action: 'sell',
            best_ask: 55,
            best_bid: 54,
            size: 100,
            fee: 0,
          },
        ],
        edge_cents: 6,
        cost_cents: 94,
        fees_cents: 1,
        annualized_return: 0.318,
        days_to_resolution: 6,
        status: 'accepted',
      },
    ],
    health: {
      connected: true,
      messages_received: 1_024,
      parse_errors: 0,
      sequence_gaps: 0,
      latency_ms: {
        p50: 2.3,
        p95: 3.7,
        p99: 4.1,
      },
      reconnections: 0,
    },
  }
}
