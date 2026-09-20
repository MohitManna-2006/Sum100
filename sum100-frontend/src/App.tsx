import { useCallback, useEffect, useMemo, useState } from 'react'
import * as Tabs from '@radix-ui/react-tabs'
import {
  adaptHealth,
  adaptOpportunity,
  rejectionMetrics,
} from './api/adapters'
import {
  getEngineControlStatus,
  startEngine,
  stopEngine,
} from './api/engineControl'
import type { HealthSnapshot } from './api/types'
import { ConnectionState } from './components/ConnectionState'
import { Header } from './components/Header'
import {
  mockCoherenceEvents,
} from './mocks/data'
import { useEngineState } from './hooks/useEngineState'
import { CoherenceView } from './views/CoherenceView'
import { HealthView } from './views/HealthView'
import { OpportunitiesView } from './views/OpportunitiesView'
import './App.css'

const emptyHealth: HealthSnapshot = {
  connected: false,
  overall: 'erroring',
  // Nothing has been received yet, so no venue can be claimed either way. The
  // engine names them once a frame arrives.
  venues: [],
  messagesReceived: 0,
  parseErrors: 0,
  gapCount: 0,
  latencyP50: 0,
  latencyP95: 0,
  latencyP99: 0,
  reconnections: 0,
}

function App() {
  const [booting, setBooting] = useState(true)
  const [controlAction, setControlAction] = useState<
    'starting' | 'stopping' | null
  >(null)
  const [controlError, setControlError] = useState<string | null>(null)
  const [managedEngine, setManagedEngine] = useState(false)
  const {
    state,
    phase,
    connected,
    error,
    malformedMessages,
    latencyHistory,
    gapHistory,
    reconnect,
    disconnect,
  } = useEngineState()

  useEffect(() => {
    const timer = window.setTimeout(() => setBooting(false), 500)
    return () => window.clearTimeout(timer)
  }, [])

  useEffect(() => {
    let active = true
    void getEngineControlStatus()
      .then((status) => {
        if (active) setManagedEngine(status.managed)
      })
      .catch(() => {
        // Production deployments may not expose the local development
        // controller; ordinary WebSocket reconnect behavior still works.
      })
    return () => {
      active = false
    }
  }, [])

  const reconnectEngine = useCallback(async () => {
    if (controlAction) return
    setControlAction('starting')
    setControlError(null)

    try {
      const status = await startEngine()
      setManagedEngine(status.managed)
    } catch (controlFailure) {
      setControlError(
        controlFailure instanceof Error
          ? controlFailure.message
          : String(controlFailure),
      )
    } finally {
      reconnect()
      setControlAction(null)
    }
  }, [controlAction, reconnect])

  const stopManagedEngine = useCallback(async () => {
    if (controlAction || !managedEngine) return
    setControlAction('stopping')
    setControlError(null)

    try {
      await stopEngine()
      setManagedEngine(false)
      disconnect()
    } catch (controlFailure) {
      setControlError(
        controlFailure instanceof Error
          ? controlFailure.message
          : String(controlFailure),
      )
    } finally {
      setControlAction(null)
    }
  }, [controlAction, disconnect, managedEngine])

  const engineOpportunities = state?.opportunities
  const engineHealth = state?.health
  const opportunities = useMemo(
    () => engineOpportunities?.map(adaptOpportunity) ?? [],
    [engineOpportunities],
  )
  const health = useMemo(
    () => (engineHealth ? adaptHealth(engineHealth, connected) : emptyHealth),
    [connected, engineHealth],
  )
  const rejections = useMemo(
    () => rejectionMetrics(engineOpportunities ?? []),
    [engineOpportunities],
  )
  const lastUpdate = state?.timestamp_ms
  const stale = Boolean(state && !connected)

  return (
    <div className={`app ${booting ? 'booting' : ''}`}>
      <div className="frame">
        <Header
          health={health}
          phase={phase}
          lastUpdate={lastUpdate}
          onReconnect={reconnectEngine}
          onStop={stopManagedEngine}
          canStop={managedEngine && connected}
          controlPending={controlAction}
        />
        {stale ? (
          <ConnectionState
            phase={phase}
            error={controlError || error}
            onReconnect={reconnectEngine}
            reconnectPending={controlAction === 'starting'}
            banner
          />
        ) : null}
        <Tabs.Root className="tabs" defaultValue="opportunities">
          <Tabs.List className="tabBar" aria-label="Dashboard views">
            <Tabs.Trigger className="tab" value="opportunities">
              Opportunities <span className="tabMeta">{state ? opportunities.length : '—'}</span>
            </Tabs.Trigger>
            <Tabs.Trigger className="tab" value="coherence">
              Coherence <span className="tabMeta">preview</span>
            </Tabs.Trigger>
            <Tabs.Trigger className="tab" value="health">
              Health <span className="tabMeta">{connected ? 'live' : 'offline'}</span>
            </Tabs.Trigger>
          </Tabs.List>

          <Tabs.Content className="content" value="opportunities">
            {state ? (
              <div className="liveRegion" inert={!connected}>
                <OpportunitiesView opportunities={opportunities} />
              </div>
            ) : (
              <ConnectionState
                phase={phase}
                error={controlError || error}
                onReconnect={reconnectEngine}
                reconnectPending={controlAction === 'starting'}
              />
            )}
          </Tabs.Content>
          <Tabs.Content className="content" value="coherence">
            <CoherenceView events={mockCoherenceEvents} />
          </Tabs.Content>
          <Tabs.Content className="content" value="health">
            {state ? (
              <div className="liveRegion" inert={!connected}>
                <HealthView
                  health={health}
                  latency={latencyHistory}
                  gaps={gapHistory}
                  rejections={rejections}
                />
              </div>
            ) : (
              <ConnectionState
                phase={phase}
                error={controlError || error}
                onReconnect={reconnectEngine}
                reconnectPending={controlAction === 'starting'}
              />
            )}
          </Tabs.Content>
        </Tabs.Root>
        <footer className="footer">
          <span>
            Paper trading · live engine stream · coherence preview is mocked
          </span>
          <span className={`sync ${connected ? '' : 'syncOffline'}`}>
            {lastUpdate
              ? `Last engine sync ${new Date(lastUpdate).toISOString().slice(11, 23)} UTC`
              : 'Waiting for engine sync'}
            {malformedMessages > 0 ? ` · ${malformedMessages} skipped` : ''}
          </span>
        </footer>
      </div>
    </div>
  )
}

export default App
