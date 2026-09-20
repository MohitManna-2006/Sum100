import { useEffect, useMemo, useState } from 'react'
import * as Tabs from '@radix-ui/react-tabs'
import {
  adaptHealth,
  adaptOpportunity,
  rejectionMetrics,
} from './api/adapters'
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
  const {
    state,
    phase,
    connected,
    error,
    malformedMessages,
    latencyHistory,
    gapHistory,
    reconnect,
  } = useEngineState()

  useEffect(() => {
    const timer = window.setTimeout(() => setBooting(false), 500)
    return () => window.clearTimeout(timer)
  }, [])

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
          onReconnect={reconnect}
        />
        {stale ? (
          <ConnectionState
            phase={phase}
            error={error}
            onReconnect={reconnect}
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
                error={error}
                onReconnect={reconnect}
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
                error={error}
                onReconnect={reconnect}
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
