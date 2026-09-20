import type { EngineHealthMetrics, EngineOpportunity, EngineState } from './client'
import type {
  GapTick,
  HealthSnapshot,
  LatencyTick,
  Opportunity,
  RejectionMetric,
} from './types'

function sameEngineOpportunity(
  previous: EngineOpportunity,
  next: EngineOpportunity,
) {
  return (
    previous.id === next.id &&
    previous.group_id === next.group_id &&
    previous.relation === next.relation &&
    previous.edge_cents === next.edge_cents &&
    previous.cost_cents === next.cost_cents &&
    previous.fees_cents === next.fees_cents &&
    previous.annualized_return === next.annualized_return &&
    previous.days_to_resolution === next.days_to_resolution &&
    previous.status === next.status &&
    previous.reject_reason === next.reject_reason &&
    previous.members.length === next.members.length &&
    previous.members.every((member, index) => member === next.members[index]) &&
    previous.legs.length === next.legs.length &&
    previous.legs.every((leg, index) => {
      const nextLeg = next.legs[index]
      return (
        leg.contract_id === nextLeg.contract_id &&
        leg.venue === nextLeg.venue &&
        leg.side === nextLeg.side &&
        leg.action === nextLeg.action &&
        leg.best_ask === nextLeg.best_ask &&
        leg.best_bid === nextLeg.best_bid &&
        leg.size === nextLeg.size &&
        leg.fee === nextLeg.fee
      )
    })
  )
}

export function sameEngineOpportunities(
  previous: EngineOpportunity[],
  next: EngineOpportunity[],
) {
  return (
    previous.length === next.length &&
    previous.every((signal, index) =>
      sameEngineOpportunity(signal, next[index]),
    )
  )
}

function displayVenue(venue: 'kalshi' | 'polymarket') {
  return venue === 'kalshi' ? 'Kalshi' : 'Polymarket'
}

export function adaptOpportunity(
  signal: EngineOpportunity,
): Opportunity {
  return {
    id: signal.id,
    pair: signal.group_id,
    event: `${signal.relation} · ${signal.members.length} contracts`,
    legs: signal.legs.map((leg) => ({
      contract: leg.contract_id,
      venue: displayVenue(leg.venue),
      action: leg.action,
      price: leg.action === 'buy' ? leg.best_ask : leg.best_bid,
      size: leg.size,
      fee: leg.fee,
    })),
    edge: signal.edge_cents,
    annualizedReturn: signal.annualized_return * 100,
    daysToResolution: signal.days_to_resolution,
    status: signal.status,
    reason: signal.reject_reason,
  }
}

export function adaptOpportunities(state: EngineState): Opportunity[] {
  return state.opportunities.map(adaptOpportunity)
}

export function adaptHealth(
  health: EngineHealthMetrics,
  transportConnected: boolean,
): HealthSnapshot {
  return {
    connected: transportConnected && health.connected,
    // A dead browser socket makes every venue reading hearsay, so the transport
    // gates the rollup the same way it gates the aggregate flag above.
    overall: transportConnected ? health.overall : 'erroring',
    venues: health.venues.map((venue) => ({
      venue: venue.venue,
      label: displayVenue(venue.venue),
      connected: transportConnected && venue.connected,
      subscribed: venue.subscribed,
      state: venue.state,
      messagesReceived: venue.messages_received,
      parseErrors: venue.parse_errors,
      reconnections: venue.reconnections,
      latencyP50: venue.latency_ms.p50,
      latencyP99: venue.latency_ms.p99,
    })),
    messagesReceived: health.messages_received,
    parseErrors: health.parse_errors,
    gapCount: health.sequence_gaps,
    latencyP50: health.latency_ms.p50,
    latencyP95: health.latency_ms.p95,
    latencyP99: health.latency_ms.p99,
    reconnections: health.reconnections,
  }
}

export function rejectionMetrics(
  opportunities: EngineOpportunity[],
): RejectionMetric[] {
  const accepted = opportunities.filter((signal) => signal.status === 'accepted').length
  const rejected = new Map<string, number>()

  opportunities.forEach((signal) => {
    if (signal.status !== 'rejected') return
    const reason = signal.reject_reason || 'Rejected'
    rejected.set(reason, (rejected.get(reason) ?? 0) + 1)
  })

  const metrics: RejectionMetric[] = [
    { label: 'Accepted', count: accepted, tone: 'gain' },
    ...Array.from(rejected, ([label, count]) => ({
      label,
      count,
      tone: 'loss' as const,
    })),
  ]

  return metrics.slice(0, 5)
}

export function appendLatencyTick(
  history: LatencyTick[],
  timestamp: number,
  value: number,
): LatencyTick[] {
  const label = new Date(timestamp).toISOString().slice(17, 21)
  return [...history, { label, value }].slice(-10)
}

export function appendGapTick(
  history: GapTick[],
  timestamp: number,
  count: number,
): GapTick[] {
  const hour = new Date(timestamp).toISOString().slice(14, 19)
  return [...history, { hour, count }].slice(-10)
}
