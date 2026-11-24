import { EnvironmentPanel } from "./EnvironmentPanel";
import { ToolsPanel } from "./ToolsPanel";

export function App() {
  return (
    <div className="shell">
      <nav className="rail" aria-label="Projects">
        <div className="rail-brand">Gantry</div>
        <p className="placeholder">Projects, workspaces and threads will appear here.</p>
      </nav>
      <main className="center">
        <p className="placeholder">Select a thread to get started.</p>
      </main>
      <aside className="dock">
        <EnvironmentPanel />
        <ToolsPanel />
      </aside>
    </div>
  );
}
