import { describe, expect, test } from "bun:test";
import type { RunProjection } from "@qltysh/fabro-api-client";

import { buildRunFailureView } from "./failure";

type State = Pick<RunProjection, "status" | "conclusion" | "stages">;

const NAMES = new Map([
  ["anthropic", "Anthropic"],
  ["openai", "OpenAI"],
]);

const CREDIT =
  "model request failed (invalid_request): provider anthropic Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits. [provider anthropic, status 400, code invalid_request_error]";

function stages(...entries: Array<[string, string, string]>): State["stages"] {
  return Object.fromEntries(
    entries.map(([key, state, started_at]) => [key, { state, started_at } as State["stages"][string]]),
  );
}

function failedRun(
  detail: Record<string, unknown>,
  { reason = "workflow_error", kind = "failed", runStages = {} as State["stages"] } = {},
): State {
  return {
    status: (kind === "failed" ? { kind, reason } : { kind }) as State["status"],
    conclusion: {
      failure: { reason, detail: { category: "deterministic", ...detail } },
    } as State["conclusion"],
    stages: runStages,
  };
}

const view = (state: State) => buildRunFailureView(state, { providerNames: NAMES });

describe("buildRunFailureView", () => {
  test("names the step the run stopped at and the provider's real message", () => {
    const failure = view(
      failedRun({ message: CREDIT }, {
        runStages: stages(["pick@1", "succeeded", "2026-10-10T18:05:42.783Z"], ["spec@1", "failed", "2026-10-10T18:41:53.513Z"]),
      }),
    );
    expect(failure).toMatchObject({
      stage: "spec",
      headline: "Anthropic declined the request for billing reasons",
      provider: "anthropic",
      message: CREDIT,
    });
    expect(failure?.nextStep).toBe("Add credit or raise the spend limit with Anthropic, then choose Retry from Actions.");
  });

  test("reads a spent quota as billing, though the provider answered 429", () => {
    const failure = view(
      failedRun({
        message: "model request failed (quota_exceeded): provider gemini Quota exceeded for metric generate_content_requests [provider gemini, status 429, code RESOURCE_EXHAUSTED]",
      }),
    );
    expect(failure?.headline).toBe("gemini declined the request for billing reasons");
  });

  test("names an unknown provider by its id", () => {
    const failure = view(
      failedRun({ message: "model request failed (authentication): provider acme bad key [provider acme, status 401, code unauthorized]" }),
    );
    expect(failure?.headline).toBe("acme rejected the API key");
  });

  test("reads auth, rate limits, timeouts and outages from the request's own status and kind", () => {
    const headline = (message: string) => view(failedRun({ message }))?.headline;
    expect(headline("model request failed (authentication): provider anthropic invalid x-api-key [provider anthropic, status 401, code authentication_error]"))
      .toBe("Anthropic rejected the API key");
    expect(headline("model request failed (access_denied): provider openai The model is not available to your organization [provider openai, status 403, code model_not_found]"))
      .toBe("OpenAI denied access to the request");
    expect(headline("model request failed (authentication): provider acme forbidden [provider acme, status 403, code forbidden]"))
      .toBe("acme rejected the API key");
    expect(headline("model request failed (rate_limit): provider openai Too many requests [provider openai, status 429, code rate_limit_exceeded]"))
      .toBe("OpenAI rate-limited the request");
    expect(headline("model request failed (server): provider anthropic Overloaded [provider anthropic, status 529, code overloaded_error]"))
      .toBe("Anthropic was unavailable");
    expect(headline("model request failed (timeout): provider openai request timed out"))
      .toBe("The request to OpenAI timed out");
  });

  test("doesn't blame the provider for failures on Fabro's side", () => {
    const failure = (message: string) => view(failedRun({ message }));
    expect(failure("model request failed (configuration): provider anthropic is disabled in the catalog")).toMatchObject({
      headline: "Fabro couldn't send the request to Anthropic",
      nextStep: "Check Anthropic and the step's model under Settings → Models, then choose Retry from Actions.",
    });
    expect(failure("model request failed (model_selection): provider anthropic has no default model")?.headline)
      .toBe("Fabro couldn't send the request to Anthropic");
    expect(failure("model request failed (network): provider openai connection refused")?.headline)
      .toBe("The request never reached OpenAI");
    expect(failure("model request failed (resource_limit): provider openai response exceeded 16 MiB")?.headline)
      .toBe("Fabro stopped the request to OpenAI");
  });

  test("falls back to the provider's status and code", () => {
    const failure = view(
      failedRun({
        message: "model request failed (invalid_request): provider openai Unsupported parameter: 'temperature' is not supported with this model. [provider openai, status 400, code unsupported_parameter]",
      }),
    );
    expect(failure?.headline).toBe("OpenAI returned 400 unsupported parameter");
  });

  test("never reads a provider, status or billing problem out of a command's own words", () => {
    const failure = view(
      failedRun({
        message: "script exited with status 401",
        exec_output_tail: { stdout: "test billing::invoice ... FAILED\nretry the provider list later\n", stderr: null },
      }),
    );
    expect(failure).toMatchObject({
      headline: "The step's command exited with status 401",
      provider: null,
      nextStep: "Its output, under Details, usually says why.",
    });
    expect(view(failedRun({ message: "Ran 512 tests; line 503 failed." }))?.headline).toBe("Ran 512 tests; line 503 failed.");
  });

  test("names no step when the run recovered from an earlier failed one", () => {
    const failure = view(
      failedRun({ message: "push rejected: remote branch moved" }, {
        reason: "publish_failed",
        runStages: stages(
          ["toolchain@1", "failed", "2026-10-10T15:20:45Z"],
          ["fixup@1", "succeeded", "2026-10-10T15:21:00Z"],
          ["verify@1", "succeeded", "2026-10-10T15:25:00.500Z"],
        ),
      }),
    );
    expect(failure?.stage).toBeNull();
    expect(failure?.headline).toBe("The steps finished, but publishing the result failed");
  });

  test("labels a looped step with its visit, as the Stages sidebar does", () => {
    const runStages = stages(["review@1", "succeeded", "2026-10-10T15:20:00Z"], ["review@3", "failed", "2026-10-10T15:30:00Z"]);
    expect(view(failedRun({ message: "script exited with status 1" }, { runStages }))?.stage).toBe("review@3");
  });

  test("names the failed branch of a fan-out, though a sibling started after it", () => {
    const runStages = stages(
      ["checks@1", "failed", "2026-10-10T15:20:00.000Z"],
      ["lint@1", "failed", "2026-10-10T15:20:00.010Z"],
      ["test@1", "cancelled", "2026-10-10T15:20:00.013Z"],
    );
    runStages["lint@1"]!.parallel_branch_id = "checks@1:0";
    runStages["test@1"]!.parallel_branch_id = "checks@1:1";
    expect(view(failedRun({ message: "script exited with status 1" }, { runStages }))?.stage).toBe("lint");

    runStages["report@1"] = { state: "succeeded", started_at: "2026-10-10T15:21:00Z" } as State["stages"][string];
    expect(view(failedRun({ message: "push rejected" }, { reason: "publish_failed", runStages }))?.stage).toBeNull();
  });

  test("shows nothing for a cancelled run, and says what happened for other endings", () => {
    expect(view(failedRun({ message: "Run cancelled before worker launch completed" }, { reason: "cancelled" }))).toBeNull();
    expect(view(failedRun({ message: "denied at review" }, { reason: "approval_denied" }))?.headline).toBe("Approval was denied");
    expect(view(failedRun({ message: "budget of $5 reached" }, { reason: "budget_exhausted" }))?.headline).toBe("The run used up its budget");
    expect(view(failedRun({ message: "sandbox image not found" }, { reason: "sandbox_init_failed" }))?.headline).toBe("The run couldn't start");
    expect(view(failedRun({ message: "sandbox connection reset" }, { reason: "transient_infra" }))).toMatchObject({
      headline: "The step hit a temporary infrastructure error",
      nextStep: "Choose Retry from Actions in a few minutes.",
    });
  });

  test("says why a dead run stopped, though a dead run records no conclusion", () => {
    const dead = {
      status: { kind: "dead" },
      conclusion: null,
      stages: stages(["build@1", "failed", "2026-10-10T18:05:42Z"]),
    } as unknown as State;
    expect(buildRunFailureView(dead, { lifecycleError: "worker exited unexpectedly" })).toMatchObject({
      stage: "build",
      headline: "Worker exited unexpectedly",
      message: "worker exited unexpectedly",
    });
    expect(buildRunFailureView(dead)?.headline).toBe("The run's worker stopped before the run finished");
  });

  test("shows the end of a command's output, from whichever stream has it", () => {
    const tail = (exec_output_tail: Record<string, unknown>) =>
      view(failedRun({ message: "script exited with status 1", exec_output_tail }))?.output;
    expect(tail({ stderr: "\n  \n", stdout: "real error\n" })).toBe("real error");
    expect(tail({ stderr: "line one\r\nline two\r\n", stdout: null })).toBe("line one\nline two");
    expect(tail({ stdout: "10%\r50%\r100% done\n" })).toBe("100% done");
    const many = Array.from({ length: 30 }, (_, i) => `line ${i + 1}`).join("\n");
    expect(tail({ stdout: many })?.split("\n")).toEqual(Array.from({ length: 12 }, (_, i) => `line ${i + 19}`));
  });

  test("falls back to the output the failed step recorded", () => {
    const runStages = stages(["test@1", "failed", "2026-10-10T18:05:42Z"]);
    runStages["test@1"]!.output = "running 3 tests\ntest parse ... FAILED\n";
    const failure = view(failedRun({ message: "script exited with status 101" }, { runStages }));
    expect(failure).toMatchObject({
      output: "running 3 tests\ntest parse ... FAILED",
      nextStep: "Its output, under Details, usually says why.",
    });

    runStages["test@1"]!.output = "blob://sha256/abc";
    expect(view(failedRun({ message: "script exited with status 101" }, { runStages }))?.output).toBeNull();

    runStages["test@1"]!.output = "I will now write the spec.";
    expect(view(failedRun({ message: CREDIT }, { runStages }))?.output).toBeNull();
  });

  test("cuts a long fallback headline without splitting a character", () => {
    const headline = view(failedRun({ message: `${"🔥".repeat(200)} and more` }))!.headline;
    expect(Array.from(headline)).toHaveLength(111);
    expect(headline.endsWith("🔥…")).toBe(true);
  });

  test("shows nothing for runs that didn't fail", () => {
    expect(buildRunFailureView({ status: { kind: "succeeded" }, stages: {} } as State)).toBeNull();
    expect(buildRunFailureView(null)).toBeNull();
  });
});
