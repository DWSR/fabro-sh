import { useRef, useState } from "react";
import { useNavigate } from "react-router";
import { useSWRConfig } from "swr";

import { mutateRunListCaches } from "../lib/board-cache";
import { queryKeys } from "../lib/query-keys";
import { mapError, retryRun } from "../lib/run-actions";
import { ConfirmDialog, ErrorMessage } from "./ui";

export function RetryRunDialog({
  runId,
  onClose,
}: {
  runId: string;
  onClose: () => void;
}) {
  const navigate = useNavigate();
  const { mutate } = useSWRConfig();
  const inFlight = useRef(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleRetry() {
    // Lock synchronously: two clicks can arrive before React renders pending.
    if (inFlight.current) return;
    inFlight.current = true;
    setPending(true);
    setError(null);
    try {
      const run = await retryRun(runId);
      // Retry returns a different run; never replace the original's detail cache.
      void mutate(queryKeys.runs.detail(run.id), run, { revalidate: false });
      mutateRunListCaches(mutate);
      onClose();
      void navigate(`/runs/${encodeURIComponent(run.id)}`);
    } catch (error) {
      setError(mapError(error, "retry"));
    } finally {
      inFlight.current = false;
      setPending(false);
    }
  }

  return (
    <ConfirmDialog
      open
      title="Retry this run?"
      description={
        <div className="space-y-3">
          <p>Retry creates a new run from the saved specification and starts from the beginning.</p>
          {error && <ErrorMessage message={error} />}
        </div>
      }
      confirmLabel="Retry run"
      pendingLabel="Creating run…"
      tone="primary"
      pending={pending}
      onConfirm={() => void handleRetry()}
      onCancel={() => {
        if (!inFlight.current) onClose();
      }}
    />
  );
}
