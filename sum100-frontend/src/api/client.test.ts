import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  createWebsocketClient,
  fetchSignalHistory,
  websocketUrl,
} from './client'
import { engineState } from '../test/engineFixture'
import { MockWebSocket } from '../test/MockWebSocket'

describe('API client', () => {
  beforeEach(() => {
    MockWebSocket.reset()
    vi.stubGlobal('WebSocket', MockWebSocket)
  })

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('builds secure and insecure websocket URLs', () => {
    expect(websocketUrl('http://localhost:8080')).toBe('ws://localhost:8080/ws')
    expect(websocketUrl('https://engine.example.com/api')).toBe(
      'wss://engine.example.com/ws',
    )
  })

  it('opens and publishes validated engine state', () => {
    const callbacks = {
      onOpen: vi.fn(),
      onUpdate: vi.fn(),
      onError: vi.fn(),
      onMalformedMessage: vi.fn(),
      onClose: vi.fn(),
    }

    createWebsocketClient(callbacks, 'ws://localhost:8080/ws')
    const socket = MockWebSocket.instances[0]

    socket.open()
    socket.message(JSON.stringify(engineState()))

    expect(callbacks.onOpen).toHaveBeenCalledOnce()
    expect(callbacks.onUpdate).toHaveBeenCalledWith(engineState())
    expect(callbacks.onMalformedMessage).not.toHaveBeenCalled()
  })

  it('skips malformed messages and continues with the next update', () => {
    const callbacks = {
      onOpen: vi.fn(),
      onUpdate: vi.fn(),
      onError: vi.fn(),
      onMalformedMessage: vi.fn(),
      onClose: vi.fn(),
    }

    createWebsocketClient(callbacks)
    const socket = MockWebSocket.instances[0]

    socket.message('{not-json')
    socket.message(JSON.stringify({ timestamp_ms: 1 }))
    socket.message(JSON.stringify(engineState()))

    expect(callbacks.onMalformedMessage).toHaveBeenCalledTimes(2)
    expect(callbacks.onUpdate).toHaveBeenCalledTimes(1)
    expect(callbacks.onUpdate).toHaveBeenLastCalledWith(engineState())
  })

  it('normalizes paginated signal history responses', async () => {
    const signal = engineState().opportunities[0]
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ signals: [signal], cursor: 'next-page' }),
    })
    vi.stubGlobal('fetch', fetchMock)

    await expect(fetchSignalHistory(100, 'page-1')).resolves.toEqual({
      signals: [signal],
      cursor: 'next-page',
    })
    expect(String(fetchMock.mock.calls[0][0])).toContain(
      '/api/signals?since=100&cursor=page-1',
    )
  })
})
