import { Suspense, use } from "react";

import { commands } from "@gantry/protocol";

type AppInfo = Awaited<ReturnType<typeof commands.appInfo>>;
type AppInfoResult = { ok: true; info: AppInfo } | { ok: false; error: string };

// The answer never changes while the app runs, so fetch it once and share the promise.
let appInfoRequest: Promise<AppInfoResult> | undefined;

function loadAppInfo(): Promise<AppInfoResult> {
  appInfoRequest ??= commands.appInfo().then(
    (info): AppInfoResult => ({ ok: true, info }),
    (error: unknown): AppInfoResult => ({ ok: false, error: String(error) }),
  );
  return appInfoRequest;
}

function EnvironmentDetails() {
  const result = use(loadAppInfo());

  if (!result.ok) {
    return <p className="error">Could not load environment info: {result.error}</p>;
  }

  const { version, os, arch, webviewVersion } = result.info;
  return (
    <dl className="facts">
      <dt>Gantry</dt>
      <dd>{version}</dd>
      <dt>OS</dt>
      <dd>{os}</dd>
      <dt>Arch</dt>
      <dd>{arch}</dd>
      <dt>WebView</dt>
      <dd>{webviewVersion ?? "unknown"}</dd>
    </dl>
  );
}

export function EnvironmentPanel() {
  return (
    <section className="panel" aria-labelledby="environment-heading">
      <h2 id="environment-heading" className="panel-title">
        Environment
      </h2>
      <Suspense fallback={<p className="placeholder">Loading…</p>}>
        <EnvironmentDetails />
      </Suspense>
    </section>
  );
}
