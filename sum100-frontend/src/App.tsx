import { useEffect, useState } from 'react'
import * as Tabs from '@radix-ui/react-tabs'
import { Header } from './components/Header'
import {
  mockCoherenceEvents,
  mockGaps,
  mockHealth,
  mockLatency,
  mockOpportunities,
  mockRejections,
} from './mocks/data'
import { CoherenceView } from './views/CoherenceView'
import { HealthView } from './views/HealthView'
import { OpportunitiesView } from './views/OpportunitiesView'
import './App.css'

function App() {
  const [booting, setBooting] = useState(true)

  useEffect(() => {
    const timer = window.setTimeout(() => setBooting(false), 500)
    return () => window.clearTimeout(timer)
  }, [])

  return (
    <div className={`app ${booting ? 'booting' : ''}`}>
      <div className="frame">
        <Header health={mockHealth} />
        <Tabs.Root className="tabs" defaultValue="opportunities">
          <Tabs.List className="tabBar" aria-label="Dashboard views">
            <Tabs.Trigger className="tab" value="opportunities">
              Opportunities <span className="tabMeta">{mockOpportunities.length}</span>
            </Tabs.Trigger>
            <Tabs.Trigger className="tab" value="coherence">
              Coherence <span className="tabMeta">{mockCoherenceEvents.length}</span>
            </Tabs.Trigger>
            <Tabs.Trigger className="tab" value="health">
              Health <span className="tabMeta">live</span>
            </Tabs.Trigger>
          </Tabs.List>

          <Tabs.Content className="content" value="opportunities">
            <OpportunitiesView opportunities={mockOpportunities} />
          </Tabs.Content>
          <Tabs.Content className="content" value="coherence">
            <CoherenceView events={mockCoherenceEvents} />
          </Tabs.Content>
          <Tabs.Content className="content" value="health">
            <HealthView
              health={mockHealth}
              latency={mockLatency}
              gaps={mockGaps}
              rejections={mockRejections}
            />
          </Tabs.Content>
        </Tabs.Root>
        <footer className="footer">
          <span>Paper trading · mocked engine data</span>
          <span className="sync">Last engine sync 21:39:42.108 UTC</span>
        </footer>
      </div>
    </div>
  )
}

export default App
