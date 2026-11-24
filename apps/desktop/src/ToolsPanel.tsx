import { Suspense, use } from "react";

import { commands } from "@gantry/protocol";

type ToolStatus = Awaited<ReturnType<typeof commands.toolHealth>>[number];
const STATE_LABEL: Record<ToolStatus["state"], string> = {
  ok: "ok",
  missing: "not found",
  shimOnly: "shim only (.cmd)",
  failed: "probe failed",
};

type ToolsResult = { ok: true; tools: ToolStatus[] } | { ok: false; error: string };

// Probed once per app session; a "re-check" action can come with the real health system.
let toolsRequest: Promise<ToolsResult> | undefined;

function loadTools(): Promise<ToolsResult> {
  toolsRequest ??= commands.toolHealth().then(
    (tools): ToolsResult => ({ ok: true, tools }),
    (error: unknown): ToolsResult => ({ ok: false, error: String(error) }),
  );
  return toolsRequest;
}

function ToolList() {
  const result = use(loadTools());

  if (!result.ok) {
    return <p className="error">Could not probe tools: {result.error}</p>;
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
  return (
    <section className="panel" aria-labelledby="tools-heading">
      <h2 id="tools-heading" className="panel-title">
        Agent CLIs
      </h2>
      <Suspense fallback={<p className="placeholder">Probing…</p>}>
        <ToolList />
      </Suspense>
    </section>
  );
}
