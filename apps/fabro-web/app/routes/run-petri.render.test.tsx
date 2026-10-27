/**
 * The run detail's views over a Petri run, rendered from the projection and
 * the stream the server tests captured (`test-fixtures/petri/*.json`): one
 * scenario per fixture, every view `VIEWS.md` lists that the web app draws
 * from those two sources.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { ReactElement } from "react";
import type { RunStage } from "@qltysh/fabro-api-client";
import TestRenderer, { act } from "react-test-renderer";
import { MemoryRouter } from "react-router";

import { RunWaterfall } from "../components/run-waterfall";
import { StageSidebar } from "../components/stage-sidebar";
import { FanInResults } from "../components/stage-renderers/fan-in-results";
import { HumanQA } from "../components/stage-renderers/human-qa";
import { ParallelChildren } from "../components/stage-renderers/parallel-children";
import { ConditionalDecision } from "../components/stage-renderers/conditional-decision";
import { loadPetriFixture, type PetriFixture } from "../lib/petri-fixtures";
import {
  debugRowsFromStream,
  deriveRunPhasesFromStream,
  findPetriEdgeForStage,
  itemsForStage,
  parallelOverviewFromProjection,
  parsePetriInterviewPairs,
  petriParsed,
  reducerTranscriptFromProjection,
  stagesFromProjection,
} from "../lib/petri-stream";
import { setupReactTestEnv } from "../lib/test-utils";
import { getObject } from "../lib/unknown";
import { StreamEventsView } from "./run-events";
import { StageChatView, buildPetriStageActivity } from "./run-stages";

let teardown: () => void;
beforeEach(() => {
  teardown = setupReactTestEnv();
});
afterEach(() => teardown());

function render(element: ReactElement): string {
  let renderer!: TestRenderer.ReactTestRenderer;
  act(() => {
    renderer = TestRenderer.create(
      <MemoryRouter initialEntries={["/runs/run-1"]}>{element}</MemoryRouter>,
    );
  });
  const json = JSON.stringify(renderer.toJSON());
  act(() => renderer.unmount());
  return json;
}

function runStages(fixture: PetriFixture): RunStage[] {
  return stagesFromProjection(fixture.projection).map((stage) => ({
    id: stage.id,
    name: stage.name,
    handler: stage.handler,
    status: stage.status,
    node_id: stage.nodeId,
    visit: stage.visit,
    started_at: stage.startedAt,
    wall_time_ms: fixture.projection.stages[stage.id]?.timing?.wall_time_ms,
    usage: stage.usage,
    parallel_group_id: stage.parallelGroupId ?? undefined,
    parallel_branch_index: stage.parallelBranchIndex ?? undefined,
  }));
}

function createdAt(fixture: PetriFixture): string {
  return new Date(fixture.stream[0].recorded_at).toISOString();
}

describe("a command-only run", () => {
  const fixture = loadPetriFixture("command");
  const stages = stagesFromProjection(fixture.projection);

  test("the stage list shows every stage with its state", () => {
    expect(stages.map((stage) => [stage.id, stage.status])).toEqual([
      ["start@1", "succeeded"],
      ["say@1", "succeeded"],
      ["exit@1", "succeeded"],
    ]);
    const html = render(<StageSidebar stages={stages} runId="run-1" />);
    for (const name of ["start", "say", "exit"]) expect(html).toContain(name);
    // The sidebar shows a stage's state as its icon's tone: mint is succeeded.
    expect((html.match(/text-mint/g) ?? []).length).toBe(3);
    expect(html).not.toContain("animate-pulse");
  });

  test("the command stage is one command turn with its exit status and output size", () => {
    const say = fixture.projection.stages["say@1"];
    const activity = buildPetriStageActivity(
      itemsForStage(fixture.stream, "say@1"),
      say,
      "command",
    );
    expect(activity.turns).toHaveLength(1);
    expect(activity.turns[0]).toMatchObject({
      kind: "command",
      running: false,
      exitCode: 0,
      outputBytes: say.output_bytes,
    });
  });

  test("the waterfall's phases come from the lifecycle records", () => {
    const html = render(
      <RunWaterfall
        runId="run-1"
        phases={deriveRunPhasesFromStream(fixture.stream, createdAt(fixture))}
        stages={runStages(fixture)}
        createdAtIso={createdAt(fixture)}
        completedAtIso={fixture.projection.conclusion?.timestamp ?? null}
      />,
    );
    for (const label of ["Submitted", "Runnable", "Initializing", "say"]) {
      expect(html).toContain(label);
    }
  });
});

describe("the hello run on the twin", () => {
  const fixture = loadPetriFixture("hello");
  const stages = stagesFromProjection(fixture.projection);

  test("the agent stage's chat shows the prompt and the agent's response", () => {
    const greet = stages.find((stage) => stage.id === "greet@1")!;
    expect(greet.handler).toBe("agent");
    const activity = buildPetriStageActivity(
      itemsForStage(fixture.stream, "greet@1"),
      fixture.projection.stages["greet@1"],
      "agent",
    );
    expect(activity.turns.map((turn) => turn.kind)).toEqual(["system", "assistant"]);
    expect(activity.turns[0]).toMatchObject({ kind: "system" });
    expect((activity.turns[0] as { content: string }).content).toContain("Add a haiku");
    expect(activity.turns[1]).toMatchObject({
      kind: "assistant",
      content: "A haiku, added.",
      inputTokens: 1,
      outputTokens: 5,
    });
    const html = render(
      <StageChatView
        turns={activity.turns}
        pendingTools={activity.pendingTools}
        stage={greet}
      />,
    );
    expect(html).toContain("A haiku, added.");
  });

  test("the stage list shows the agent stage succeeded", () => {
    const greet = stages.find((stage) => stage.id === "greet@1")!;
    expect(greet.status).toBe("succeeded");
    expect(greet.providerUsed?.model).toBe("gpt-5.4");
    const html = render(<StageSidebar stages={stages} runId="run-1" />);
    expect(html).toContain("greet");
    expect(html).toContain("text-mint");
  });
});

describe("a two-branch parallel run", () => {
  const fixture = loadPetriFixture("parallel");
  const stages = stagesFromProjection(fixture.projection);

  test("the fork lists both branches under it with their outcomes", () => {
    const fork = stages.find((stage) => stage.id === "fork@1")!;
    const html = render(
      <ParallelChildren
        stage={fork}
        events={[]}
        overview={parallelOverviewFromProjection(fixture.projection.stages["fork@1"])}
        runId="run-1"
        allStages={stages}
      />,
    );
    expect(html).toContain("Branches");
    expect(html).toContain("/runs/run-1/stages/a%401");
    expect(html).toContain("/runs/run-1/stages/b%401");
    expect((html.match(/Succeeded/g) ?? []).length).toBeGreaterThanOrEqual(2);
  });

  test("the fan-in joined the branches", () => {
    const merge = stages.find((stage) => stage.id === "merge@1")!;
    const html = render(
      <FanInResults
        stage={merge}
        events={[]}
        reducer={reducerTranscriptFromProjection(fixture.projection.stages["merge@1"])}
      />,
    );
    expect(html).toContain("Joined");
    expect(html).not.toContain("Reducer transcript");
  });

  test("the events view lists Petri events, platform notices and checkpoints", () => {
    const html = render(
      <StreamEventsView
        rows={debugRowsFromStream(fixture.stream)}
        error={undefined}
        onRetry={() => {}}
        runStart={createdAt(fixture)}
        view="events"
        onChangeView={() => {}}
      />,
    );
    for (const name of ["run.started", "visit.started", "fork.completed", "run.finished"]) {
      expect(html).toContain(name);
    }
    expect(html).toContain("run.notice");
    expect(html).toContain("checkpoint");
    expect(html).toContain('"data-stage":"a@1"');
    expect(html).toContain(`${fixture.stream.length} items`);
  });

  test("the sidebar groups the branches under the fork", () => {
    const branches = stages.filter((stage) => stage.parallelGroupId === "fork@1");
    expect(branches.map((stage) => stage.id)).toEqual(["a@1", "b@1"]);
    const html = render(<StageSidebar stages={stages} runId="run-1" />);
    for (const name of ["fork", "a", "b", "merge"]) expect(html).toContain(name);
  });
});

describe("a human gate answered through the API", () => {
  const fixture = loadPetriFixture("gate");
  const stages = stagesFromProjection(fixture.projection);

  test("the Q&A shows the question, its options and the answer with who gave it", () => {
    const gate = stages.find((stage) => stage.id === "gate@1")!;
    expect(gate.handler).toBe("human");
    const html = render(
      <HumanQA
        stage={gate}
        events={[]}
        pairs={parsePetriInterviewPairs(fixture.stream)}
      />,
    );
    expect(html).toContain("Go?");
    expect(html).toContain("[Y] Yes");
    expect(html).toContain("[N] No");
    expect(html).toContain("dev");
    expect(html).not.toContain("pending");
  });

  test("the gate's decision took the no edge", () => {
    const gate = stages.find((stage) => stage.id === "gate@1")!;
    const edge = findPetriEdgeForStage(fixture.stream, "gate@1");
    const html = render(
      <ConditionalDecision
        stage={gate}
        runEvents={[]}
        edge={edge}
        allStages={stages}
        runId="run-1"
      />,
    );
    expect(html).toContain("/runs/run-1/stages/no%401");
    expect(html).not.toContain("No target");
  });

  test("no question is left pending in the projection", () => {
    expect(Object.keys(fixture.projection.pending_interviews)).toEqual([]);
    expect(stages.map((stage) => stage.id)).toEqual(["start@1", "gate@1", "no@1", "exit@1"]);
  });

  test.each([
    ["https://example.com/review/%2Fdraft?revision=one&token=a%2Bb#feedback", true],
    ["javascript:alert(1)", false],
    ["https://user:password@example.com/review", false],
    ["/relative/review", false],
    ["ftp://example.com/review", false],
  ])("answered Q&A preserves safe review links after replay: %s", (url, linked) => {
    // Add a reference to the real API-captured gate stream; keep its delivered
    // answer and settled projection, which has no pending interview left.
    const stream = structuredClone(fixture.stream);
    const question = stream.map((item) => getObject(petriParsed(item), "question")).find(Boolean)!;
    question.text = "Review the design document, then choose the next action.";
    question.reference = { label: "Design review", url, kind: "document" };
    const gate = stages.find((stage) => stage.id === "gate@1")!;

    for (const events of [stream, JSON.parse(JSON.stringify(stream))]) {
      let renderer!: TestRenderer.ReactTestRenderer;
      act(() => {
        renderer = TestRenderer.create(
          <HumanQA stage={gate} pairs={parsePetriInterviewPairs(events)} />,
        );
      });
      try {
        const links = renderer.root.findAllByType("a");
        expect(links).toHaveLength(linked ? 1 : 0);
        if (linked) {
          expect(links[0].props).toMatchObject({
            href: url,
            target: "_blank",
            rel: "noopener noreferrer",
            referrerPolicy: "no-referrer",
          });
          expect(links[0].findByType("span").children).toEqual(["Design review"]);
        } else {
          expect(JSON.stringify(renderer.toJSON())).toContain(question.text);
        }
        expect(JSON.stringify(renderer.toJSON())).toContain("Answered");
        expect(JSON.stringify(renderer.toJSON())).toContain("dev");
      } finally {
        act(() => renderer.unmount());
      }
    }
  });
});
