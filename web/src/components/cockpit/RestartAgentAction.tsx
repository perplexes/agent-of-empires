// Small button + destructive-confirm dialog for restarting the
// cockpit worker against the current adapter binary, preserving the
// transcript via `session/load`. Use case: the user just upgraded
// `claude-agent-acp` (or any other adapter) on disk and wants the
// in-memory agent subprocess to pick up the new binary without
// taking down `aoe serve`.
//
// Parallels SwitchSubstrateAction in shape (icon button → confirm
// dialog → POST → optimistic close). Distinct endpoint:
// /api/sessions/{id}/cockpit/restart-agent (see api/cockpit.rs:
// restart_cockpit_agent).

import { useEffect, useRef, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { restartCockpitAgent } from "../../lib/api";
import { OFFLINE_TITLE, useServerDown } from "../../lib/connectionState";

interface Props {
  sessionId: string;
  /** Optional className override on the trigger button. */
  className?: string;
  /** Render style: an icon button (compact, for toolbars) or a full
   *  button with text (for banner contexts). */
  variant?: "icon" | "button";
}

export function RestartAgentAction({
  sessionId,
  className,
  variant = "icon",
}: Props) {
  const offline = useServerDown();
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!confirmOpen) return;
    const onClick = (e: MouseEvent) => {
      if (!dialogRef.current?.contains(e.target as Node)) setConfirmOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setConfirmOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [confirmOpen]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await restartCockpitAgent(sessionId);
      setConfirmOpen(false);
      setBusy(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  const triggerDisabled = offline;
  const triggerLabel = "Restart cockpit agent";

  return (
    <>
      <button
        type="button"
        title={offline ? OFFLINE_TITLE : triggerLabel}
        aria-label={triggerLabel}
        disabled={triggerDisabled}
        onClick={() => setConfirmOpen(true)}
        data-testid="restart-agent-trigger"
        className={
          className ??
          [
            variant === "button"
              ? "inline-flex items-center gap-1.5 rounded-md border border-surface-700 bg-surface-800 px-2.5 py-1.5 text-[12px] font-medium text-text-secondary hover:bg-surface-700"
              : "inline-flex h-7 w-7 items-center justify-center rounded-md text-text-dim hover:bg-surface-800 hover:text-text-secondary",
            "transition-colors disabled:cursor-not-allowed disabled:opacity-50",
          ].join(" ")
        }
      >
        <RefreshCw className="h-3.5 w-3.5" />
        {variant === "button" && <span>Restart agent</span>}
      </button>

      {confirmOpen && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40"
          role="dialog"
          aria-modal="true"
        >
          <div
            ref={dialogRef}
            className="w-[26rem] max-w-[92vw] rounded-xl border border-surface-700 bg-surface-900 p-4 shadow-xl"
          >
            <h2 className="text-sm font-semibold text-text-primary">
              Restart cockpit agent?
            </h2>
            <p className="mt-2 text-xs leading-relaxed text-text-muted">
              Tears down the agent subprocess and respawns it against the
              current adapter binary on disk. The transcript is preserved on
              disk and resumed via{" "}
              <code className="rounded bg-surface-950 px-1 font-mono text-[11px]">
                session/load
              </code>
              ; pending tool calls and in-flight prompts are cancelled. Use
              after upgrading{" "}
              <code className="rounded bg-surface-950 px-1 font-mono text-[11px]">
                claude-agent-acp
              </code>{" "}
              or another adapter so the new version takes effect without an{" "}
              <code className="rounded bg-surface-950 px-1 font-mono text-[11px]">
                aoe serve
              </code>{" "}
              restart.
            </p>
            {error && (
              <p className="mt-2 rounded bg-rose-950/40 px-2 py-1 text-xs text-rose-300">
                {error}
              </p>
            )}
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirmOpen(false)}
                disabled={busy}
                className="rounded-md border border-surface-700 bg-surface-800 px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-60"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void submit()}
                disabled={busy}
                data-testid="restart-agent-confirm"
                className="inline-flex items-center gap-1.5 rounded-md bg-brand-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-70"
              >
                {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                Restart
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
