import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { RunStreamItem } from "@qltysh/fabro-api-client";
import TestRenderer, { act } from "react-test-renderer";
import { MemoryRouter } from "react-router";

import { findPetriEdgeForStage } from "../../lib/petri-stream";
import { makeStage, setupReactTestEnv, textContent } from "../../lib/test-utils";
import { ConditionalDecision } from "./conditional-decision";

let teardown: () => void;
let renderer: TestRenderer.ReactTestRenderer | undefined;
beforeEach(() => { teardown = setupReactTestEnv(); });
afterEach(() => {
  if (renderer) act(() => renderer!.unmount());
  renderer = undefined;
  teardown();
});

// The reported decision: the false branch goes to a; the default goes to z.
function route(edge = 2, visit = 1) {
  return {
    run_id: "run-1", stream_seq: visit, kind: "petri", id: String(visit), recorded_at: 1,
    item: {
      subject: {
        node: {
          name: "gate", kind: "noop",
          meta: {
            kind: "conditional",
            edges: {
              "1": { to: "a", label: "Special", condition: "never=true" },
              "2": { to: "z", label: "Default" },
            } as Record<string, unknown> | undefined,
          },
        },
        visit,
      },
      record: { body: { event: "route.applied", kind: "edge", edge: edge as unknown } },
      derived: { target: { name: edge === 1 ? "a" : "z" } },
    },
  } satisfies RunStreamItem;
}

function renderDecision(stream: RunStreamItem[], visit = 1) {
  const stage = makeStage({ id: `gate@${visit}`, nodeId: "gate", handler: "conditional", visit });
  const targets = ["a", "z"].map((nodeId) => makeStage({ id: `${nodeId}@1`, nodeId }));
  act(() => {
    renderer = TestRenderer.create(
      <MemoryRouter>
        <ConditionalDecision
          stage={stage}
          edge={findPetriEdgeForStage(stream, stage.id)}
          allStages={targets}
          runId="run-1"
        />
      </MemoryRouter>,
    );
  });
  return {
    details: renderer!.root.findAllByType("dd").map(textContent),
    target: renderer!.root.findByType("a").props.href,
  };
}

describe("Decision reason from the selected route", () => {
  test.each([false, true])("an unconditional fallback is the default edge (replayed: %s)", (replay) => {
    const stream = [route()];
    expect(renderDecision(replay ? JSON.parse(JSON.stringify(stream)) : stream)).toEqual({
      details: ["Default edge"], target: "/runs/run-1/stages/z%401",
    });
  });

  test("a matched condition keeps its expression and target", () => {
    const item = route(1);
    item.item.subject.node.meta.edges!["1"] = {
      to: "a", label: "Special", condition: "outcome=succeeded",
    };
    expect(renderDecision([item])).toEqual({
      details: ["Matched condition", "outcome=succeeded"], target: "/runs/run-1/stages/a%401",
    });
  });

  test.each([
    ["missing table", undefined],
    ["missing selected edge", { "1": { to: "a", condition: "never=true" } }],
    ["malformed selected edge", { "2": [] }],
    ["missing target metadata", { "2": {} }],
    ["mismatched target metadata", { "2": { to: "a" } }],
    ["malformed condition", { "2": { to: "z", condition: false } }],
  ])("%s keeps the target without inventing a reason", (_name, edges) => {
    const item = route();
    item.item.subject.node.meta.edges = edges;
    expect(renderDecision([item])).toEqual({
      details: ["Reason unavailable"], target: "/runs/run-1/stages/z%401",
    });
  });

  test("a missing edge id cannot identify the fallback", () => {
    const item = route();
    item.item.record.body.edge = undefined;
    expect(renderDecision([item]).details).toEqual(["Reason unavailable"]);
  });

  test("a human choice without a condition is not called a default", () => {
    const item = route();
    item.item.subject.node.meta.kind = "human";
    expect(renderDecision([item]).details).toEqual(["Reason unavailable"]);
  });

  test("the edge id distinguishes routes sharing a destination", () => {
    const item = route();
    item.item.subject.node.meta.edges!["1"] = { to: "z", condition: "never=true" };
    expect(renderDecision([item]).details).toEqual(["Default edge"]);
  });

  test("a later visit does not replace the selected visit's reason", () => {
    const first = route(2, 1);
    const second = route(1, 2);
    expect(renderDecision([first, second], 1).details).toEqual(["Default edge"]);
    expect(findPetriEdgeForStage([first, second], "gate@2")).toMatchObject({
      toNode: "a", reason: "condition", condition: "never=true",
    });
  });
});
