import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { engineState } from '../test/engineFixture'
import { MockWebSocket } from '../test/MockWebSocket'
import {
  RECONNECT_BASE_MS,
  UPDATE_INTERVAL_MS,
  useEngineState,
} from './useEngineState'

describe('useEngineState', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.setSystemTime(1_789_000_000_000)
    MockWebSocket.reset()
    vi.stubGlobal('WebSocket', MockWebSocket)
  })

  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it('connects, publishes state, and coalesces burst updates', () => {
    const { result, unmount } = renderHook(() => useEngineState())
    const socket = MockWebSocket.instances[0]

    act(() => socket.open())
    expect(result.current.connected).toBe(true)

    act(() => socket.message(JSON.stringify(engineState())))
    expect(result.current.state?.health.messages_received).toBe(1_024)

    const second = engineState(1_789_000_000_010)
    second.health.messages_received = 1_025
    const latest = engineState(1_789_000_000_020)
    latest.health.messages_received = 1_026

    act(() => {
      socket.message(JSON.stringify(second))
      socket.message(JSON.stringify(latest))
    })
    expect(result.current.state?.health.messages_received).toBe(1_024)

    act(() => vi.advanceTimersByTime(UPDATE_INTERVAL_MS))
    expect(result.current.state?.health.messages_received).toBe(1_026)
    expect(result.current.latencyHistory).toHaveLength(2)

    unmount()
  })

  it('reconnects after a lost connection', () => {
    const { result, unmount } = renderHook(() => useEngineState())
    const firstSocket = MockWebSocket.instances[0]

    act(() => firstSocket.open())
    act(() => firstSocket.disconnect('engine restart'))

    expect(result.current.connected).toBe(false)
    expect(result.current.phase).toBe('reconnecting')
    expect(result.current.error).toBe('engine restart')

    act(() => vi.advanceTimersByTime(RECONNECT_BASE_MS))
    expect(MockWebSocket.instances).toHaveLength(2)

    act(() => MockWebSocket.instances[1].open())
    expect(result.current.connected).toBe(true)
    expect(result.current.error).toBeNull()

    unmount()
  })

  it('stays offline after an intentional stop until reconnect is requested', () => {
    const { result, unmount } = renderHook(() => useEngineState())

    act(() => MockWebSocket.instances[0].open())
    act(() => result.current.disconnect())

    expect(result.current.connected).toBe(false)
    expect(result.current.phase).toBe('offline')

    act(() => vi.advanceTimersByTime(RECONNECT_BASE_MS * 4))
    expect(MockWebSocket.instances).toHaveLength(1)

    act(() => result.current.reconnect())
    expect(MockWebSocket.instances).toHaveLength(2)

    unmount()
  })

  it('counts malformed messages without dropping the connection', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const { result, unmount } = renderHook(() => useEngineState())
    const socket = MockWebSocket.instances[0]

    act(() => {
      socket.open()
      socket.message('not json')
    })

    expect(result.current.connected).toBe(true)
    expect(result.current.malformedMessages).toBe(1)
    expect(result.current.state).toBeNull()
    expect(warn).toHaveBeenCalledOnce()

    unmount()
  })
})
