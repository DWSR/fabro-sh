import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import type { ReactNode } from "react";
import TestRenderer, { act } from "react-test-renderer";
import { createMemoryRouter, Outlet, RouterProvider } from "react-router";
import { loadPetriFixture } from "../lib/petri-fixtures";
import { makeStage, setupReactTestEnv, textContent } from "../lib/test-utils";

import { ParallelChildren } from "../components/stage-renderers/parallel-children";
import { ConditionalDecision } from "../components/stage-renderers/conditional-decision";
import { StagePopover } from "../components/stage-popover";
import { RunWaterfall } from "../components/run-waterfall";

const ids = [
  "reviewer@1",
  "reviewer/e2@1",
  "reviewer/50%@1",
  "reviewer%20x@1",
  "reviewer%2Fe2@1",
  "reviewer%252Fe2@1",
  "reviewer?x#y@1",
  "réview/日本@1",
];
const fixture = loadPetriFixture("command");
const stages = ids.map((id) => ({
  id,
  name: id.slice(0, -2),
  node_id: id.slice(0, -2),
  visit: 1,
  handler: "command",
  status: "succeeded",
  usage: fixture.projection.stages["say@1"].usage,
}));
const projection = {
  ...fixture.projection,
  stages: Object.fromEntries(
    ids.map((id) => [id, fixture.projection.stages["say@1"]]),
  ),
};
// Keep real Petri records and give each occurrence a visibly distinct command.
const stream = ids.flatMap((id) =>
  fixture.stream
    .filter(
      (entry) =>
        entry.kind === "petri" && entry.item.subject?.node?.name === "say",
    )
    .map((entry) => {
      const copy = structuredClone(entry);
      copy.item.subject.node.name = id.slice(0, -2);
      copy.item.subject.node.meta.script = `echo selected:${id}`;
      return copy;
    }),
);
let loading = false;
let available = stages;
const logRequests: string[] = [];
mock.module("../lib/queries", () => ({
  useRunGraph: () => ({ data: undefined }),
  useRunGraphSource: () => ({ data: undefined }),
  useRun: () => ({ data: undefined }),
  useRunStages: () => ({
    data: loading ? undefined : { data: available },
    isLoading: loading,
  }),
  useRunState: () => ({ data: projection }),
  useRunStream: () => ({ data: stream }),
  useRunStageContextWindow: () => ({ data: undefined }),
  useRunStageLog: (_runId: string, stageId: string) => {
    logRequests.push(stageId);
    const output = Buffer.from(`output:${stageId}`, "utf8");
    return {
      data: {
        bytes_base64: output.toString("base64"),
        next_offset: output.length,
        total_bytes: output.length,
      },
    };
  },
}));
mock.module("../components/run-summary-panel", () => ({
  RunSummaryPanel: () => null,
}));
let graphClick: (id: string) => void;
mock.module("../hooks/use-annotated-run-graph-svg", () => ({
  useAnnotatedRunGraphSvg: ({
    onStageClick,
  }: {
    onStageClick: (id: string) => void;
  }) => {
    graphClick = onStageClick;
  },
}));
const { default: RunOverview } = await import("./run-overview");
const { default: RunStages } = await import("./run-stages");
let renderer: TestRenderer.ReactTestRenderer;
let teardown: () => void;
beforeEach(() => {
  teardown = setupReactTestEnv();
  loading = false;
  available = stages;
  logRequests.length = 0;
});
afterEach(() => {
  if (renderer) act(() => renderer.unmount());
  teardown();
});

function render(entry = "/runs/run-1/stages", source?: ReactNode) {
  const router = createMemoryRouter(
    [
      {
        path: "/runs/:id",
        element: <Outlet />,
        children: [
          { index: true, element: source ?? <RunOverview /> },
          { path: "stages", element: <RunStages /> },
          { path: "stages/:stageId", element: <RunStages /> },
        ],
      },
    ],
    { initialEntries: [entry] },
  );
  act(() => {
    renderer = TestRenderer.create(<RouterProvider router={router} />);
  });
  return router;
}
async function clickStage(id: string) {
  const link = renderer.root
    .findAllByType("a")
    .find(
      (node) =>
        node.props.href.includes("/stages/") &&
        node
          .findAllByType("span")
          .some((span) => textContent(span) === id.slice(0, -2)),
    );
  expect(link).toBeDefined();
  await act(async () =>
    link!.props.onClick({
      button: 0,
      defaultPrevented: false,
      preventDefault() {
        this.defaultPrevented = true;
      },
    }),
  );
}
function expectSelected(id: string) {
  const text = textContent(renderer.root);
  expect(text).toContain(`echo selected:${id}`);
  expect(text).toContain(`output:${id}`);
  expect(logRequests.at(-1)).toBe(id);
  const active = renderer.root
    .findAllByType("a")
    .filter((node) => node.props.className?.includes("bg-overlay text-fg"));
  expect(active).toHaveLength(1);
  expect(active[0].props.href).toBe(
    `/runs/run-1/stages/${encodeURIComponent(id)}`,
  );
  for (const other of ids.filter((value) => value !== id)) {
    expect(text).not.toContain(`echo selected:${other}`);
    expect(text).not.toContain(`output:${other}`);
  }
}

describe("stage occurrence navigation", () => {
  test.each(ids)("clicking the sidebar opens exactly %s", async (id) => {
    const router = render();
    await clickStage(id);
    expect(router.state.location.pathname).toBe(
      `/runs/run-1/stages/${encodeURIComponent(id)}`,
    );
    expectSelected(id);
  });
  test.each(ids)("direct links preserve %s through the nested route", (id) => {
    render(`/runs/run-1/stages/${encodeURIComponent(id)}/?view=detail#output`);
    expectSelected(id);
  });
  test("back and forward restore the exact occurrence", async () => {
    const router = render();
    await clickStage(ids[1]);
    await clickStage(ids[4]);
    await act(async () => {
      await router.navigate(-1);
    });
    expectSelected(ids[1]);
    await act(async () => {
      await router.navigate(1);
    });
    expectSelected(ids[4]);
  });
  test("the stage index still selects the first stage", () => {
    render();
    expectSelected(ids[0]);
  });
  test.each(["missing%401", "bad%ZZ"])(
    "an explicit unavailable ID %s never selects another stage",
    (segment) => {
      render(`/runs/run-1/stages/${segment}`);
      expect(textContent(renderer.root)).toContain("Stage not found");
      expect(logRequests).toEqual([]);
    },
  );
  test("an explicit ID waits for the stage list then selects it", async () => {
    loading = true;
    const router = render(`/runs/run-1/stages/${encodeURIComponent(ids[1])}`);
    expect(textContent(renderer.root)).toContain("Loading stages");
    expect(textContent(renderer.root)).not.toContain("Stage not found");
    loading = false;
    await act(async () => {
      await router.navigate(router.state.location.pathname, { replace: true });
    });
    expectSelected(ids[1]);
  });
});

describe("other stage navigation surfaces", () => {
  const target = makeStage({
    id: ids[1],
    name: "reviewer/e2",
    nodeId: "reviewer/e2",
    handler: "command",
    status: "succeeded",
    parallelGroupId: "fork@1",
    parallelBranchIndex: 0,
  });
  const source = makeStage({
    id: "fork@1",
    nodeId: "fork",
    status: "succeeded",
  });
  const producers = [
    [
      "parallel child",
      <ParallelChildren
        stage={source}
        runId="run-1"
        allStages={[target]}
        overview={{ branchCount: 1, results: [] }}
      />,
    ],
    [
      "conditional target",
      <ConditionalDecision
        stage={source}
        runId="run-1"
        allStages={[target]}
        edge={{
          fromNode: "fork",
          toNode: target.nodeId,
          reason: "unconditional",
          condition: null,
        }}
      />,
    ],
    [
      "resumed-stage popover",
      <StagePopover
        stage={{ ...source, resumedFromStageId: target.id }}
        runId="run-1"
        duration="1s"
      />,
    ],
    [
      "waterfall",
      <RunWaterfall
        runId="run-1"
        phases={[]}
        stages={[
          {
            ...stages[1],
            started_at: "2026-10-20T12:00:00Z",
            wall_time_ms: 1000,
          },
        ]}
        createdAtIso="2026-10-20T12:00:00Z"
        completedAtIso="2026-10-20T12:00:02Z"
      />,
    ],
  ] as const;
  test.each(producers)(
    "%s opens the offered occurrence",
    async (_name, element) => {
      const router = render("/runs/run-1", element);
      const link = renderer.root
        .findAllByType("a")
        .find((node) => node.props.href.includes("/stages/"));
      expect(link).toBeDefined();
      await act(async () =>
        link!.props.onClick({
          button: 0,
          defaultPrevented: false,
          preventDefault() {
            this.defaultPrevented = true;
          },
        }),
      );
      expect(router.state.location.pathname).toBe(
        `/runs/run-1/stages/${encodeURIComponent(target.id)}`,
      );
      expectSelected(target.id);
    },
  );
  test("the resumed-stage banner opens the source occurrence", async () => {
    available = stages.map((stage, index) =>
      index === 0 ? { ...stage, resumed_from_stage_id: ids[1] } : stage,
    );
    render();
    const link = renderer.root
      .findAllByType("a")
      .find((node) => textContent(node) === ids[1]);
    await act(async () =>
      link!.props.onClick({
        button: 0,
        defaultPrevented: false,
        preventDefault() {
          this.defaultPrevented = true;
        },
      }),
    );
    expectSelected(ids[1]);
  });
  test("the graph stage-click handler navigates to the offered occurrence", async () => {
    const router = render("/runs/run-1");
    await act(async () => graphClick(ids[1]));
    expect(router.state.location.pathname).toBe(
      `/runs/run-1/stages/${encodeURIComponent(ids[1])}`,
    );
    expectSelected(ids[1]);
  });
});
