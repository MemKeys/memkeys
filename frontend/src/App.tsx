import { useState } from 'react';
import FactsBrowser from './components/FactsBrowser';
import MemoryGraph from './components/MemoryGraph';
import IdentityChip from './components/IdentityChip';
import LoginPage from './components/LoginPage';
import { ConfigProvider } from './services/config';
import { useAuth } from './services/authContext';
import { hasSession } from './services/session';
import './App.css';

type Tab = 'facts' | 'graph';

function AppShell() {
  const { auth } = useAuth();
  const [activeTab, setActiveTab] = useState<Tab>('facts');

  // Sign-in gate (ISI-6580): the login page is shown until a session exists
  // (and again after any API 401 clears it).
  if (!hasSession(auth)) return <LoginPage />;

  return (
    <div className="app">
      <header className="header">
        <h1>🔍 MemKeys Inspector</h1>
        <IdentityChip />
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
  );
}

function App() {
  return (
    <ConfigProvider>
      <AppShell />
    </ConfigProvider>
  );
}

export default App;
