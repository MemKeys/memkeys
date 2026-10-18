import { useState } from 'react';
import FactsBrowser from './components/FactsBrowser';
import MemoryGraph from './components/MemoryGraph';
import ConnectionBar from './components/ConnectionBar';
import { ConfigProvider } from './services/config';
import './App.css';

type Tab = 'facts' | 'graph';

function App() {
  const [activeTab, setActiveTab] = useState<Tab>('facts');

  return (
    <ConfigProvider>
      <div className="app">
        <header className="header">
          <h1>🔍 MemKeys Inspector</h1>
          <ConnectionBar />
        </header>

        <nav className="nav-tabs">
          <button
            className={`nav-tab ${activeTab === 'facts' ? 'active' : ''}`}
            onClick={() => setActiveTab('facts')}
          >
            Facts Browser
          </button>
          <button
            className={`nav-tab ${activeTab === 'graph' ? 'active' : ''}`}
            onClick={() => setActiveTab('graph')}
          >
            Memory Graph
          </button>
        </nav>

        <main className="content">
          {activeTab === 'facts' && <FactsBrowser />}
          {activeTab === 'graph' && <MemoryGraph />}
        </main>
      </div>
    </ConfigProvider>
  );
}

export default App;
