/** Inline error with a retry action, for panels that load once via a cached promise. */
export function LoadError({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="load-error">
      <p className="error">{message}</p>
      <button type="button" className="retry" onClick={onRetry}>
        Retry
      </button>
    </div>
  );
}
