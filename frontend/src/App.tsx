import { useState } from 'react';
import FactsBrowser from './components/FactsBrowser';
import MemoryGraph from './components/MemoryGraph';
import ConnectionBar from './components/ConnectionBar';
import ExploreBrowser from './components/ExploreBrowser';
import { ConfigProvider } from './services/config';
import './App.css';

// ISI-6581: 'explore' (keyless POST /memory/search) is the default landing
// tab; the keyed Facts Browser stays as the advanced 'facts' tab.
type Tab = 'explore' | 'facts' | 'graph';

function App() {
  const [activeTab, setActiveTab] = useState<Tab>('explore');

  return (
    <ConfigProvider>
      <div className="app">
        <header className="header">
          <h1>🔍 MemKeys Inspector</h1>
          <ConnectionBar />
        </header>

        <nav className="nav-tabs">
          <button
            className={`nav-tab ${activeTab === 'explore' ? 'active' : ''}`}
            onClick={() => setActiveTab('explore')}
          >
            Explore
          </button>
          <button
            className={`nav-tab ${activeTab === 'facts' ? 'active' : ''}`}
            onClick={() => setActiveTab('facts')}
          >
            Keyed lookup
          </button>
          <button
            className={`nav-tab ${activeTab === 'graph' ? 'active' : ''}`}
            onClick={() => setActiveTab('graph')}
          >
            Memory Graph
          </button>
        </nav>

        <main className="content">
          {activeTab === 'explore' && (
            <ExploreBrowser onUseKeyedLookup={() => setActiveTab('facts')} />
          )}
          {activeTab === 'facts' && <FactsBrowser />}
          {activeTab === 'graph' && <MemoryGraph />}
        </main>
      </div>
    </ConfigProvider>
  );
}

export default App;
