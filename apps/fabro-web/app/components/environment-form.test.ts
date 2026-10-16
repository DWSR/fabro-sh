import { describe, expect, test } from "bun:test";
import { createElement, useState } from "react";
import TestRenderer, { act } from "react-test-renderer";
import type { Environment, EnvironmentResourcesSettings } from "@qltysh/fabro-api-client";

import { setupReactTestEnv } from "../lib/test-utils";
import {
  EMPTY_ENVIRONMENT_FORM,
  EnvironmentFormFields,
  createRequestFromForm,
  environmentToFormValues,
  isEnvironmentFormValid,
  replaceRequestFromForm,
  type EnvironmentFormValues,
} from "./environment-form";

function form(overrides: Partial<EnvironmentFormValues>): EnvironmentFormValues {
  return { ...EMPTY_ENVIRONMENT_FORM, id: "docker", ...overrides };
}

describe("environment image source", () => {
  test("image source requires a non-empty image reference", () => {
    expect(isEnvironmentFormValid(form({ imageSource: "image", dockerRef: "" }))).toBe(false);
    expect(
      isEnvironmentFormValid(form({ imageSource: "image", dockerRef: "ubuntu:24.04" })),
    ).toBe(true);
  });

  test("dockerfile source requires non-empty Dockerfile contents", () => {
    expect(isEnvironmentFormValid(form({ imageSource: "dockerfile", dockerfile: "" }))).toBe(false);
    expect(
      isEnvironmentFormValid(form({ imageSource: "dockerfile", dockerfile: "FROM ubuntu" })),
    ).toBe(true);
  });

  test("an empty Dockerfile does not satisfy the image-reference source", () => {
    expect(
      isEnvironmentFormValid(form({ imageSource: "image", dockerRef: "", dockerfile: "FROM x" })),
    ).toBe(false);
  });

  test("image source sends only the docker reference", () => {
    const request = createRequestFromForm(
      form({ imageSource: "image", dockerRef: "ubuntu:24.04", dockerfile: "FROM leftover" }),
    );
    expect(request.image.docker).toBe("ubuntu:24.04");
    expect(request.image.dockerfile).toBeNull();
  });

  test("dockerfile source sends only the inline Dockerfile", () => {
    const request = createRequestFromForm(
      form({ imageSource: "dockerfile", dockerRef: "leftover", dockerfile: "FROM ubuntu" }),
    );
    expect(request.image.docker).toBeNull();
    expect(request.image.dockerfile?.value).toBe("FROM ubuntu");
  });
});

function environment(resources: EnvironmentResourcesSettings): Environment {
  return {
    id: "roundtrip",
    revision: "revision-1",
    provider: "docker",
    image: { docker: "ubuntu:24.04", dockerfile: null },
    resources,
    network: { mode: "allow_all", allow: [] },
    lifecycle: { preserve: false, stop_on_terminal: true, auto_stop: null },
    labels: { owner: "api", purpose: "roundtrip" },
    env: { TZ: "UTC" },
  };
}

describe("environment resource round trips", () => {
  const cases: EnvironmentResourcesSettings[] = [
    // API response for 512MiB; regression originally reported in #952.
    { cpu: null, memory: "536870912B", disk: null },
    { cpu: 4, memory: "4GB", disk: "8GB" },
    { cpu: null, memory: null, disk: null },
    { cpu: 32, memory: "64GB", disk: "2TB" },
    { cpu: 1, memory: "512MiB", disk: "1GiB" },
    { cpu: 2, memory: "1.5GB", disk: "512MB" },
    { cpu: 2, memory: "1B", disk: "1TiB" },
  ];

  for (const resources of cases) {
    test(`no-op save preserves ${JSON.stringify(resources)}`, () => {
      const original = environment(resources);
      const { id, revision, ...settings } = original;
      expect(replaceRequestFromForm(environmentToFormValues(original))).toEqual(settings);
    });

    test(`unrelated edits preserve ${JSON.stringify(resources)} and labels`, () => {
      const original = environment(resources);
      const values = environmentToFormValues(original);
      const saved = replaceRequestFromForm({
        ...values,
        dockerRef: "ubuntu:26.04",
        preserve: true,
        autoStop: "30m",
        envVars: [...values.envVars, { key: "EDITOR", value: "vi" }],
      });
      expect(saved.resources).toEqual(resources);
      expect(saved.labels).toEqual(original.labels);
      expect(saved.image.docker).toBe("ubuntu:26.04");
      expect(saved.lifecycle).toEqual({ preserve: true, stop_on_terminal: true, auto_stop: "30m" });
      expect(saved.env).toEqual({ TZ: "UTC", EDITOR: "vi" });
    });
  }

  test("editing one resource preserves the other resources", () => {
    const values = environmentToFormValues(environment(cases[0]));
    expect(replaceRequestFromForm({ ...values, cpu: 6 }).resources).toEqual({
      ...cases[0], cpu: 6,
    });
    expect(replaceRequestFromForm({ ...values, memory: "6GB" }).resources).toEqual({
      ...cases[0], memory: "6GB",
    });
    expect(replaceRequestFromForm({ ...values, disk: "12GB" }).resources).toEqual({
      ...cases[0], disk: "12GB",
    });
  });

  test("new environments keep the existing resource defaults", () => {
    expect(createRequestFromForm(form({ dockerRef: "ubuntu:24.04" })).resources)
      .toEqual({ cpu: 4, memory: "8GB", disk: "16GB" });
  });
});

test("sliders preserve loaded resources until that specific slider changes", () => {
  const cleanup = setupReactTestEnv();
  let renderer: TestRenderer.ReactTestRenderer | undefined;
  let saved: ReturnType<typeof replaceRequestFromForm> | undefined;
  function Editor() {
    const [values, setValues] = useState(() => environmentToFormValues(environment({
      cpu: null, memory: "536870912B", disk: null,
    })));
    saved = replaceRequestFromForm(values);
    return createElement(EnvironmentFormFields, { values, onChange: setValues, lockId: true });
  }
  try {
    act(() => { renderer = TestRenderer.create(createElement(Editor)); });
    const input = (label: string) => renderer!.root.findByProps({ "aria-label": label });
    for (const [label, min, max] of [["CPU", 1, 8], ["Memory", 1, 16], ["Disk", 1, 20]] as const) {
      expect(input(label).props.type).toBe("range");
      expect(input(label).props.min).toBe(min);
      expect(input(label).props.max).toBe(max);
      expect(input(label).props.step).toBe(1);
    }
    expect(input("CPU").props["aria-valuetext"]).toBe("Provider default");
    expect(input("Memory").props.value).toBe(1);
    expect(input("Memory").props["aria-valuetext"]).toBe("536870912B");
    expect(input("Disk").props["aria-valuetext"]).toBe("Provider default");
    act(() => { input("Image reference").props.onChange({ target: { value: "alpine:latest" } }); });
    expect(saved!.resources).toEqual({ cpu: null, memory: "536870912B", disk: null });
    expect(saved!.labels).toEqual({ owner: "api", purpose: "roundtrip" });
    act(() => { input("CPU").props.onChange({ target: { value: "6" } }); });
    expect(saved!.resources).toEqual({ cpu: 6, memory: "536870912B", disk: null });
    // Selecting the displayed fallback is an explicit edit, even when the
    // numeric position matches the initial projection of the loaded value.
    act(() => { input("Memory").props.onChange({ target: { value: "2" } }); });
    act(() => { input("Memory").props.onChange({ target: { value: "1" } }); });
    expect(saved!.resources).toEqual({ cpu: 6, memory: "1GB", disk: null });
    act(() => { input("Disk").props.onChange({ target: { value: "12" } }); });
    expect(saved!.resources).toEqual({ cpu: 6, memory: "1GB", disk: "12GB" });
  } finally {
    act(() => { renderer?.unmount(); });
    cleanup();
  }
});
