import { useCallback, useEffect, useState } from 'react'
import {
  appendGapTick,
  appendLatencyTick,
  sameEngineOpportunities,
} from '../api/adapters'
import {
  createWebsocketClient,
  type EngineState,
} from '../api/client'
import type { ConnectionPhase, GapTick, LatencyTick } from '../api/types'

export const UPDATE_INTERVAL_MS = 100
export const RECONNECT_BASE_MS = 500
export const RECONNECT_MAX_MS = 10_000

export function useEngineState() {
  const [state, setState] = useState<EngineState | null>(null)
  const [phase, setPhase] = useState<ConnectionPhase>('connecting')
  const [error, setError] = useState<string | null>(null)
  const [malformedMessages, setMalformedMessages] = useState(0)
  const [connectionKey, setConnectionKey] = useState(0)
  const [latencyHistory, setLatencyHistory] = useState<LatencyTick[]>([])
  const [gapHistory, setGapHistory] = useState<GapTick[]>([])

  useEffect(() => {
    let disposed = false
    let socket: WebSocket | null = null
    const reconnectTimers = new Set<number>()
    let publishTimer: number | null = null
    let pendingState: EngineState | null = null
    let reconnectAttempt = 0
    let lastPublishedAt = 0

    const clearReconnectTimers = () => {
      for (const timer of reconnectTimers) window.clearTimeout(timer)
      reconnectTimers.clear()
    }

    const publish = (nextState: EngineState) => {
      if (disposed) return
      lastPublishedAt = Date.now()
      pendingState = null
      setState((current) =>
        current &&
        sameEngineOpportunities(current.opportunities, nextState.opportunities)
          ? { ...nextState, opportunities: current.opportunities }
          : nextState,
      )
      setLatencyHistory((history) =>
        appendLatencyTick(
          history,
          nextState.timestamp_ms,
          nextState.health.latency_ms.p50,
        ),
      )
      setGapHistory((history) =>
        appendGapTick(
          history,
          nextState.timestamp_ms,
          nextState.health.sequence_gaps,
        ),
      )
    }

    const queueUpdate = (nextState: EngineState) => {
      const elapsed = Date.now() - lastPublishedAt
      if (lastPublishedAt === 0 || elapsed >= UPDATE_INTERVAL_MS) {
        publish(nextState)
        return
      }

      pendingState = nextState
      if (publishTimer !== null) return

      publishTimer = window.setTimeout(() => {
        publishTimer = null
        if (pendingState) publish(pendingState)
      }, UPDATE_INTERVAL_MS - elapsed)
    }

    const connect = () => {
      if (disposed) return
      if (
        socket?.readyState === WebSocket.OPEN ||
        socket?.readyState === WebSocket.CONNECTING
      ) {
        clearReconnectTimers()
        return
      }
      clearReconnectTimers()
      setPhase(reconnectAttempt === 0 ? 'connecting' : 'reconnecting')

      let client: WebSocket
      client = createWebsocketClient({
        onOpen: () => {
          if (disposed || socket !== client) return
          clearReconnectTimers()
          reconnectAttempt = 0
          setPhase('connected')
          setError(null)
        },
        onUpdate: (nextState) => {
          if (!disposed && socket === client) queueUpdate(nextState)
        },
        onError: (message) => {
          if (disposed || socket !== client) return
          setError(message)
        },
        onMalformedMessage: (message) => {
          if (disposed || socket !== client) return
          console.warn(`[Sum100] ${message}`)
          setMalformedMessages((count) => count + 1)
        },
        onClose: (event) => {
          if (disposed || socket !== client) return
          socket = null
          setPhase('reconnecting')
          setError(event.reason || 'Connection lost; retrying automatically')

          const delay = Math.min(
            RECONNECT_BASE_MS * 2 ** reconnectAttempt,
            RECONNECT_MAX_MS,
          )
          reconnectAttempt += 1
          const timer = window.setTimeout(() => {
            reconnectTimers.delete(timer)
            connect()
          }, delay)
          reconnectTimers.add(timer)
        },
      })
      socket = client
    }

    connect()

    return () => {
      disposed = true
      clearReconnectTimers()
      if (publishTimer !== null) window.clearTimeout(publishTimer)
      if (
        socket &&
        (socket.readyState === WebSocket.OPEN ||
          socket.readyState === WebSocket.CONNECTING)
      ) {
        socket.close()
      }
    }
  }, [connectionKey])

  const reconnect = useCallback(() => {
    setError(null)
    setPhase('connecting')
    setConnectionKey((key) => key + 1)
  }, [])

  return {
    state,
    phase,
    connected: phase === 'connected',
    error,
    malformedMessages,
    latencyHistory,
    gapHistory,
    reconnect,
  }
}
