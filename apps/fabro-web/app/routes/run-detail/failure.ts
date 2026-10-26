import {
  FailureCategory,
  FailureReason,
  type ExecOutputTail,
  type RunProjection,
} from "@qltysh/fabro-api-client";

/** Lines of a failed command's output shown on the page. */
const OUTPUT_LINES = 12;
/** Fallback headlines are cut to this many characters. */
const HEADLINE_CHARS = 110;

export interface RunFailureView {
  /** The step the run stopped at (`spec`, `review@3`), when it stopped at one. */
  stage: string | null;
  headline: string;
  nextStep: string;
  /** The recorded failure message, verbatim. */
  message: string;
  /** Provider id, when the failure is a model request the provider refused. */
  provider: string | null;
  /** Last OUTPUT_LINES of the failed command's stderr, else its stdout. */
  output: string | null;
}

type FailureState = Pick<RunProjection, "status" | "conclusion" | "stages">;

/**
 * Reads the failure from run state (`conclusion.failure`); the run summary the
 * status popover reads doesn't carry the message or the command's output.
 */
export function buildRunFailureView(
  state: FailureState | null | undefined,
  providerNames: ReadonlyMap<string, string> = new Map(),
): RunFailureView | null {
  const kind = state?.status?.kind;
  if (!state || (kind !== "failed" && kind !== "dead")) return null;
  const failure = state.conclusion?.failure;
  if (!failure?.detail.message || failure.reason === FailureReason.CANCELLED) return null;

  const { message, category } = failure.detail;
  const output = outputTail(failure.detail.exec_output_tail);
  const request = parseModelRequest(message);
  const name = (id: string) => providerNames.get(id) ?? id;
  const advice =
    reasonAdvice(failure.reason, category) ??
    (request ? requestAdvice(request, message, name(request.provider)) : stepAdvice(message, category, output));
  return {
    stage: stoppedAt(state.stages),
    ...advice,
    message,
    provider: request?.provider ?? null,
    output,
  };
}

type Advice = Pick<RunFailureView, "headline" | "nextStep">;

const RETRY = "choose Retry from Actions";

function reasonAdvice(reason: FailureReason, category: FailureCategory): Advice | null {
  if (reason === FailureReason.APPROVAL_DENIED) {
    return { headline: "Approval was denied", nextStep: `The run stops where it was denied. To run it again, ${RETRY}.` };
  }
  if (reason === FailureReason.BUDGET_EXHAUSTED || category === FailureCategory.BUDGET_EXHAUSTED) {
    return { headline: "The run used up its budget", nextStep: `Raise the run's budget, then ${RETRY}.` };
  }
  if (reason === FailureReason.TERMINATED) {
    return { headline: "The run was terminated", nextStep: `To run it again, ${RETRY}.` };
  }
  if (reason === FailureReason.PUBLISH_FAILED) {
    return {
      headline: "The steps finished, but publishing the result failed",
      nextStep: "The work is on the run branch. The message under Details says why publishing failed.",
    };
  }
  if (
    reason === FailureReason.LAUNCH_FAILED ||
    reason === FailureReason.BOOTSTRAP_FAILED ||
    reason === FailureReason.SANDBOX_INIT_FAILED
  ) {
    return { headline: "The run couldn't start", nextStep: "The message under Details says why." };
  }
  return null;
}

interface ModelRequest {
  provider: string;
  kind: string | null;
  status: number | null;
  code: string | null;
}

/**
 * A refused model request, as the server records it:
 * `model request failed (<kind>): provider <id> <text> [provider <id>, status <n>, code <code>]`.
 * Only this shape is read, so numbers and words in a command's output can't
 * pass for a provider's status or name.
 */
function parseModelRequest(message: string): ModelRequest | null {
  const lead = /^model request failed \(([a-z_]+)\): provider (\S+)/i.exec(message);
  const tail = /\[provider ([^,\]]+), status (\d{3})(?:, code ([^\]]+))?\]\s*$/i.exec(message);
  const provider = tail?.[1] ?? lead?.[2];
  if (!provider) return null;
  return {
    provider: provider.toLowerCase(),
    kind: lead?.[1]?.toLowerCase() ?? null,
    status: tail ? Number(tail[2]) : null,
    code: tail?.[3] ?? null,
  };
}

function requestAdvice({ kind, status, code }: ModelRequest, message: string, who: string): Advice {
  if (/credit balance|insufficient[_ ]quota|payment required|billing|spend limit/i.test(message) || status === 402) {
    return {
      headline: `${who} declined the request for billing reasons`,
      nextStep: `Add credit or raise the spend limit with ${who}, then ${RETRY}.`,
    };
  }
  if (kind === "authentication" || status === 401 || status === 403) {
    return { headline: `${who} rejected the API key`, nextStep: `Check the key under Settings → Secrets, then ${RETRY}.` };
  }
  if (kind === "rate_limit" || status === 429) {
    return { headline: `${who} rate-limited the request`, nextStep: "This is usually temporary. Choose Retry from Actions in a few minutes." };
  }
  if (kind === "timeout" || /timed out/i.test(message)) {
    return { headline: `The request to ${who} timed out`, nextStep: "This is usually temporary. Choose Retry from Actions in a few minutes." };
  }
  if ((status !== null && status >= 500) || /overloaded/i.test(message)) {
    return { headline: `${who} was unavailable`, nextStep: "This is usually temporary. Choose Retry from Actions in a few minutes." };
  }
  const what = [status, code?.replace(/_/g, " ")].filter(Boolean).join(" ");
  return {
    headline: what ? `${who} returned ${what}` : `${who} rejected the request`,
    nextStep: `${who}'s message, under Details, says why.`,
  };
}

function stepAdvice(message: string, category: FailureCategory, output: string | null): Advice {
  const exit = /exited with (?:status|code) (\d+)/i.exec(message);
  if (exit) {
    return {
      headline: `The step's command exited with status ${exit[1]}`,
      nextStep: output ? "Its output, under Details, usually says why." : "Open the step's logs for its output.",
    };
  }
  if (category === FailureCategory.TRANSIENT_INFRA) {
    return { headline: "The step hit a temporary infrastructure error", nextStep: "Choose Retry from Actions in a few minutes." };
  }
  return { headline: firstSentence(message), nextStep: "Open the failed step's logs for details." };
}

/**
 * The last step to start, if it failed. An earlier failure that a later step
 * recovered from isn't where the run stopped, so it isn't named.
 */
function stoppedAt(stages: FailureState["stages"]): string | null {
  let last: [string, number] | null = null;
  for (const [key, stage] of Object.entries(stages)) {
    const started = stage.started_at ? Date.parse(stage.started_at) : NaN;
    if (!Number.isNaN(started) && (!last || started > last[1])) last = [key, started];
  }
  if (!last || stages[last[0]]?.state !== "failed") return null;
  return last[0].replace(/@1$/, "");
}

function outputTail(tail: ExecOutputTail | null | undefined): string | null {
  const stream = [tail?.stderr, tail?.stdout].find((text) => text?.trim());
  if (!stream) return null;
  // Carriage-return progress lines keep only what was drawn last.
  const lines = stream.trimEnd().split(/\r?\n/).map((line) => line.slice(line.lastIndexOf("\r") + 1));
  return lines.slice(-OUTPUT_LINES).join("\n");
}

function firstSentence(message: string): string {
  const sentence = message.split(/(?<=[.!?])\s+/)[0]!.trim().replace(/\s+/g, " ");
  const chars = Array.from(sentence.charAt(0).toUpperCase() + sentence.slice(1));
  return chars.length > HEADLINE_CHARS ? `${chars.slice(0, HEADLINE_CHARS).join("").trimEnd()}…` : chars.join("");
}
