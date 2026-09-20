export type Relation =
  | 'complement'
  | 'exhaustive'
  | 'monotone'
  | 'equivalent'
  | 'constraint'
export type EngineSignalStatus = 'accepted' | 'rejected'

export interface EngineLeg {
  contract_id: string
  venue: 'kalshi' | 'polymarket'
  side: 'yes' | 'no'
  action: 'buy' | 'sell'
  best_ask: number
  best_bid: number
  size: number
  fee: number
}

export interface EngineOpportunity {
  id: string
  group_id: string
  relation: Relation
  members: string[]
  legs: EngineLeg[]
  edge_cents: number
  cost_cents: number
  fees_cents: number
  annualized_return: number
  days_to_resolution: number
  status: EngineSignalStatus
  reject_reason?: string
}

export interface EngineHealthMetrics {
  connected: boolean
  messages_received: number
  parse_errors: number
  sequence_gaps: number
  latency_ms: {
    p50: number
    p95: number
    p99: number
  }
  reconnections: number
}

export interface EngineState {
  timestamp_ms: number
  opportunities: EngineOpportunity[]
  health: EngineHealthMetrics
}

export interface SignalHistoryPage {
  signals: EngineOpportunity[]
  cursor?: string
}

interface WebsocketCallbacks {
  onOpen: () => void
  onUpdate: (state: EngineState) => void
  onError: (error: string) => void
  onMalformedMessage: (error: string) => void
  onClose: (event: CloseEvent) => void
}

const DEFAULT_API_URL = 'http://localhost:8080'

export const API_BASE_URL = (
  import.meta.env.VITE_API_URL || DEFAULT_API_URL
).replace(/\/+$/, '')

export function websocketUrl(apiUrl = API_BASE_URL) {
  const url = new URL('/ws', apiUrl)
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
  return url.toString()
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null

const isFiniteNumber = (value: unknown): value is number =>
  typeof value === 'number' && Number.isFinite(value)

function isEngineLeg(value: unknown): value is EngineLeg {
  if (!isRecord(value)) return false

  return (
    typeof value.contract_id === 'string' &&
    (value.venue === 'kalshi' || value.venue === 'polymarket') &&
    (value.side === 'yes' || value.side === 'no') &&
    (value.action === 'buy' || value.action === 'sell') &&
    isFiniteNumber(value.best_ask) &&
    isFiniteNumber(value.best_bid) &&
    isFiniteNumber(value.size) &&
    isFiniteNumber(value.fee)
  )
}

function isEngineOpportunity(value: unknown): value is EngineOpportunity {
  if (!isRecord(value)) return false

  return (
    typeof value.id === 'string' &&
    typeof value.group_id === 'string' &&
    ['complement', 'exhaustive', 'monotone', 'equivalent', 'constraint'].includes(
      String(value.relation),
    ) &&
    Array.isArray(value.members) &&
    value.members.every((member) => typeof member === 'string') &&
    Array.isArray(value.legs) &&
    value.legs.every(isEngineLeg) &&
    isFiniteNumber(value.edge_cents) &&
    isFiniteNumber(value.cost_cents) &&
    isFiniteNumber(value.fees_cents) &&
    isFiniteNumber(value.annualized_return) &&
    isFiniteNumber(value.days_to_resolution) &&
    (value.status === 'accepted' || value.status === 'rejected') &&
    (value.reject_reason === undefined || typeof value.reject_reason === 'string')
  )
}

function isEngineHealth(value: unknown): value is EngineHealthMetrics {
  if (!isRecord(value) || !isRecord(value.latency_ms)) return false

  return (
    typeof value.connected === 'boolean' &&
    isFiniteNumber(value.messages_received) &&
    isFiniteNumber(value.parse_errors) &&
    isFiniteNumber(value.sequence_gaps) &&
    isFiniteNumber(value.latency_ms.p50) &&
    isFiniteNumber(value.latency_ms.p95) &&
    isFiniteNumber(value.latency_ms.p99) &&
    isFiniteNumber(value.reconnections)
  )
}

export function isEngineState(value: unknown): value is EngineState {
  if (!isRecord(value)) return false

  return (
    isFiniteNumber(value.timestamp_ms) &&
    Array.isArray(value.opportunities) &&
    value.opportunities.every(isEngineOpportunity) &&
    isEngineHealth(value.health)
  )
}

export function createWebsocketClient(
  callbacks: WebsocketCallbacks,
  url = websocketUrl(),
) {
  const ws = new WebSocket(url)

  ws.onopen = () => {
    callbacks.onOpen()
  }
  ws.onmessage = (event) => {
    try {
      if (typeof event.data !== 'string') {
        callbacks.onMalformedMessage('Malformed message: expected text frame')
        return
      }

      const data: unknown = JSON.parse(event.data)
      if (!isEngineState(data)) {
        callbacks.onMalformedMessage('Malformed message: invalid engine state')
        return
      }

      callbacks.onUpdate(data)
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error)
      callbacks.onMalformedMessage(`Parse error: ${detail}`)
    }
  }
  ws.onerror = () => {
    callbacks.onError('WebSocket transport error')
  }
  ws.onclose = (event) => {
    callbacks.onClose(event)
  }

  return ws
}

export async function fetchSignalHistory(
  since: number,
  cursor?: string,
): Promise<SignalHistoryPage> {
  const url = new URL('/api/signals', API_BASE_URL)
  url.searchParams.set('since', String(since))
  if (cursor) url.searchParams.set('cursor', cursor)

  const response = await fetch(url)
  if (!response.ok) throw new Error(`HTTP ${response.status}`)

  const data: unknown = await response.json()
  if (Array.isArray(data) && data.every(isEngineOpportunity)) {
    return { signals: data }
  }

  if (
    isRecord(data) &&
    Array.isArray(data.signals) &&
    data.signals.every(isEngineOpportunity) &&
    (data.cursor === undefined || typeof data.cursor === 'string')
  ) {
    return {
      signals: data.signals,
      cursor: data.cursor,
    }
  }

  throw new Error('Invalid signal history response')
}
