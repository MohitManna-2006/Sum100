export type SignalStatus = 'accepted' | 'rejected' | 'pending'
export type TradeAction = 'buy' | 'sell'
export type SortKey = 'return' | 'edge' | 'days'

export interface SignalLeg {
  contract: string
  venue: string
  action: TradeAction
  price: number
  size: number
  fee: number
}

export interface Opportunity {
  id: string
  pair: string
  event: string
  legs: SignalLeg[]
  edge: number
  annualizedReturn: number
  daysToResolution: number
  status: SignalStatus
  reason?: string
  detectedAt: string
}

export interface HealthSnapshot {
  connected: boolean
  messagesPerSecond: number
  gapCount: number
  latencyP50: number
  latencyP99: number
  uptime: string
  contractsTracked: number
}

export interface CoherenceOutcome {
  label: string
  price: number
  fee?: number
  midpoint?: number
}

export interface LadderPoint {
  strike: number
  price: number
  violated?: boolean
}

export interface CoherenceEvent {
  id: string
  name: string
  venue: string
  outcomes: CoherenceOutcome[]
  totalPrice: number
  target: number
  gap: number
  totalFees: number
  status: 'coherent' | 'incoherent' | 'degraded'
  summary: string
  strikes: LadderPoint[]
}

export interface LatencyTick {
  label: string
  value: number
}

export interface GapTick {
  hour: string
  count: number
}

export interface RejectionMetric {
  label: string
  count: number
  tone: 'gain' | 'loss' | 'info'
}
