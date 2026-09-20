export type EngineControlState =
  | 'stopped'
  | 'starting'
  | 'running'
  | 'stopping'
  | 'error'

export interface EngineControlStatus {
  state: EngineControlState
  managed: boolean
  apiReachable: boolean
  error: string | null
}

const CONTROL_BASE = '/__sum100/engine'
const CONTROL_HEADERS = {
  'X-Sum100-Control': 'local-dashboard',
}

function isEngineControlStatus(value: unknown): value is EngineControlStatus {
  if (!value || typeof value !== 'object') return false
  const status = value as Record<string, unknown>
  return (
    ['stopped', 'starting', 'running', 'stopping', 'error'].includes(
      String(status.state),
    ) &&
    typeof status.managed === 'boolean' &&
    typeof status.apiReachable === 'boolean' &&
    (status.error === null || typeof status.error === 'string')
  )
}

async function request(
  action: 'status' | 'start' | 'stop',
  method: 'GET' | 'POST',
) {
  const response = await fetch(`${CONTROL_BASE}/${action}`, {
    method,
    headers: CONTROL_HEADERS,
    cache: 'no-store',
  })
  const data: unknown = await response.json().catch(() => null)

  if (!response.ok) {
    const detail =
      data &&
      typeof data === 'object' &&
      'error' in data &&
      typeof data.error === 'string'
        ? data.error
        : `HTTP ${response.status}`
    throw new Error(detail)
  }
  if (!isEngineControlStatus(data)) {
    throw new Error('Invalid response from the local engine controller')
  }
  return data
}

export function getEngineControlStatus() {
  return request('status', 'GET')
}

export function startEngine() {
  return request('start', 'POST')
}

export function stopEngine() {
  return request('stop', 'POST')
}
