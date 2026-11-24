import { Suspense, use, useState } from "react";

import { commands } from "@gantry/protocol";

import { LoadError } from "./LoadError";

type ToolStatus = Awaited<ReturnType<typeof commands.toolHealth>>[number];
const STATE_LABEL: Record<ToolStatus["state"], string> = {
  ok: "ok",
  missing: "not found",
  shimOnly: "shim only (.cmd)",
  failed: "probe failed",
};

type ToolsResult = { ok: true; tools: ToolStatus[] } | { ok: false; error: string };

// Probed once per app session. A failure stays cached (React re-reads the same promise after it
// settles) until Retry clears it.
let toolsRequest: Promise<ToolsResult> | undefined;

function loadTools(): Promise<ToolsResult> {
  toolsRequest ??= commands.toolHealth().then(
    (tools): ToolsResult => ({ ok: true, tools }),
    (error: unknown): ToolsResult => ({ ok: false, error: String(error) }),
  );
  return toolsRequest;
}

function ToolList({ onRetry }: { onRetry: () => void }) {
  const result = use(loadTools());

  if (!result.ok) {
    return <LoadError message={`Could not probe tools: ${result.error}`} onRetry={onRetry} />;
  }

  return (
    <ul className="tools">
      {result.tools.map((tool) => {
        const healthy = tool.state === "ok";
        const tooltip = [tool.path, tool.error].filter(Boolean).join("\n") || undefined;
        return (
          <li key={tool.program} className="tool" title={tooltip}>
            <span className={healthy ? "dot dot-ok" : "dot dot-missing"} aria-hidden="true" />
            <span className="tool-name">{tool.name}</span>
            <span className={healthy ? "tool-detail" : "tool-detail error"}>
              {healthy ? tool.version : STATE_LABEL[tool.state]}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

export function ToolsPanel() {
  const [attempt, setAttempt] = useState(0);
  const retry = () => {
    toolsRequest = undefined;
    setAttempt((n) => n + 1);
  };
  return (
    <section className="panel" aria-labelledby="tools-heading">
      <h2 id="tools-heading" className="panel-title">
        Agent CLIs
      </h2>
      <Suspense fallback={<p className="placeholder">Probing…</p>}>
        <ToolList key={attempt} onRetry={retry} />
      </Suspense>
    </section>
  );
}
