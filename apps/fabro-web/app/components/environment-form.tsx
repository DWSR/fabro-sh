import { Disclosure, DisclosureButton, DisclosurePanel, Switch } from "@headlessui/react";
import { ChevronRightIcon } from "@heroicons/react/20/solid";
import {
  EnvironmentApiDockerfileSourceInlineTypeEnum,
  EnvironmentNetworkMode,
} from "@qltysh/fabro-api-client";
import type {
  CreateEnvironmentRequest,
  Environment,
  EnvironmentApiImageSettings,
  EnvironmentLifecycleSettings,
  EnvironmentNetworkSettings,
  EnvironmentResourcesSettings,
  ReplaceEnvironmentRequest,
} from "@qltysh/fabro-api-client";

import { DOCKER_PROVIDER, isCloneBasedProvider } from "../lib/environment-providers";
import { Label, Panel, Row } from "./settings-panel";
import { INPUT_CLASS } from "./ui";
import {
  KeyValueEditor,
  entriesFromMap,
  mapFromEntries,
  type KeyValueEntry,
} from "./key-value-editor";

// Parse the `provider` query param used by the create flow into a creatable
// provider, defaulting to Docker for anything that cannot back a managed
// environment. Kind names are validated server-side on create.
const PROVIDER_KIND_PATTERN = /^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$/;

export function parseCreatableProvider(value: string | null): string {
  if (value && PROVIDER_KIND_PATTERN.test(value) && isCloneBasedProvider(value)) {
    return value;
  }
  return DOCKER_PROVIDER;
}

// Environment ids are server-managed file names: lowercase, digits, hyphens.
const ENVIRONMENT_ID_PATTERN = /^[a-z0-9][a-z0-9-]{0,62}$/;

// An environment image comes from exactly one source: a prebuilt image
// reference or an inline Dockerfile. The form keeps both field values around so
// switching back and forth doesn't lose typed text, and this discriminator
// decides which one is shown, required, and sent.
type ImageSource = "image" | "dockerfile";

export interface EnvironmentFormValues {
  id: string;
  provider: string;
  imageSource: ImageSource;
  dockerRef: string;
  dockerfile: string;
  // Keep API resource values intact; blank size inputs represent provider defaults.
  cpu: number | null;
  memory: string;
  disk: string;
  blockNetwork: boolean;
  preserve: boolean;
  stopOnTerminal: boolean;
  autoStop: string;
  // Labels are not editable in the web UI — they're managed through the REST
  // API only. The form carries the loaded value verbatim so saving an edited
  // environment preserves any API-set labels instead of clearing them.
  labels: { [key: string]: string };
  envVars: KeyValueEntry[];
}

export const EMPTY_ENVIRONMENT_FORM: EnvironmentFormValues = {
  id:             "",
  provider:       DOCKER_PROVIDER,
  imageSource:    "image",
  dockerRef:      "",
  dockerfile:     "",
  cpu:            4,
  memory:         "8GB",
  disk:           "16GB",
  blockNetwork:   false,
  preserve:       false,
  stopOnTerminal: true,
  autoStop:       "",
  labels:         {},
  envVars:        [],
};

export function environmentToFormValues(environment: Environment): EnvironmentFormValues {
  return {
    id:             environment.id,
    provider:       environment.provider,
    imageSource:    environment.image.dockerfile ? "dockerfile" : "image",
    dockerRef:      environment.image.docker ?? "",
    dockerfile:     environment.image.dockerfile?.value ?? "",
    cpu:            environment.resources.cpu,
    memory:         environment.resources.memory ?? "",
    disk:           environment.resources.disk ?? "",
    blockNetwork:   environment.network.mode === EnvironmentNetworkMode.BLOCK,
    preserve:       environment.lifecycle.preserve,
    stopOnTerminal: environment.lifecycle.stop_on_terminal,
    autoStop:       environment.lifecycle.auto_stop ?? "",
    labels:         environment.labels,
    envVars:        entriesFromMap(environment.env),
  };
}

export function isEnvironmentFormValid(values: EnvironmentFormValues): boolean {
  if (!ENVIRONMENT_ID_PATTERN.test(values.id.trim())) return false;
  return imageSourceValue(values).trim() !== "";
}

// The currently selected image source's text, used both for validation and to
// drive which field is rendered as required.
function imageSourceValue(values: EnvironmentFormValues): string {
  return values.imageSource === "dockerfile" ? values.dockerfile : values.dockerRef;
}

// The Advanced disclosure (Network + Lifecycle) starts open when any of its
// values deviate from the defaults, so editing an environment never hides
// settings the operator already configured.
function hasNonDefaultAdvanced(values: EnvironmentFormValues): boolean {
  return (
    values.blockNetwork !== EMPTY_ENVIRONMENT_FORM.blockNetwork ||
    values.preserve !== EMPTY_ENVIRONMENT_FORM.preserve ||
    values.stopOnTerminal !== EMPTY_ENVIRONMENT_FORM.stopOnTerminal ||
    values.autoStop.trim() !== ""
  );
}

export function createRequestFromForm(values: EnvironmentFormValues): CreateEnvironmentRequest {
  return { id: values.id.trim(), ...settingsFromForm(values) };
}

export function replaceRequestFromForm(values: EnvironmentFormValues): ReplaceEnvironmentRequest {
  return settingsFromForm(values);
}

function settingsFromForm(values: EnvironmentFormValues): ReplaceEnvironmentRequest {
  return {
    provider:  values.provider,
    image:     imageFromForm(values),
    resources: resourcesFromForm(values),
    network:   networkFromForm(values),
    lifecycle: lifecycleFromForm(values),
    labels:    values.labels,
    env:       mapFromEntries(values.envVars),
  };
}

function imageFromForm(values: EnvironmentFormValues): EnvironmentApiImageSettings {
  if (values.imageSource === "dockerfile") {
    return {
      docker: null,
      dockerfile: {
        type:  EnvironmentApiDockerfileSourceInlineTypeEnum.INLINE,
        value: values.dockerfile,
      },
    };
  }
  return {
    docker:     values.dockerRef.trim() || null,
    dockerfile: null,
  };
}

function resourcesFromForm(values: EnvironmentFormValues): EnvironmentResourcesSettings {
  return {
    cpu:    values.cpu,
    memory: values.memory.trim() || null,
    disk:   values.disk.trim() || null,
  };
}

function networkFromForm(values: EnvironmentFormValues): EnvironmentNetworkSettings {
  return {
    mode:  values.blockNetwork ? EnvironmentNetworkMode.BLOCK : EnvironmentNetworkMode.ALLOW_ALL,
    allow: [],
  };
}

function lifecycleFromForm(values: EnvironmentFormValues): EnvironmentLifecycleSettings {
  return {
    preserve:         values.preserve,
    stop_on_terminal: values.stopOnTerminal,
    auto_stop:        values.autoStop.trim() || null,
  };
}

function parseImageSource(value: string): ImageSource {
  return value === "dockerfile" ? "dockerfile" : "image";
}

interface EnvironmentFormFieldsProps {
  values: EnvironmentFormValues;
  onChange: (values: EnvironmentFormValues) => void;
  lockId?: boolean;
}

export function EnvironmentFormFields({
  values,
  onChange,
  lockId = false,
}: EnvironmentFormFieldsProps) {
  function patch(partial: Partial<EnvironmentFormValues>) {
    onChange({ ...values, ...partial });
  }

  const idValid = ENVIRONMENT_ID_PATTERN.test(values.id.trim());

  return (
    <>
      <Panel title="General">
        <Row
          title={<Label required>ID</Label>}
          help="Lowercase identifier (letters, digits, hyphens). Runs select this environment by id. Cannot be changed after creation."
        >
          {lockId ? (
            <div className="font-mono text-sm text-fg">{values.id}</div>
          ) : (
            <input
              type="text"
              name="id"
              aria-label="Environment ID"
              value={values.id}
              onChange={(e) => patch({ id: e.target.value })}
              placeholder="fabro-dev"
              autoComplete="off"
              spellCheck={false}
              className={`${INPUT_CLASS} font-mono`}
            />
          )}
        </Row>
        <Row
          title={<Label required>Source</Label>}
          help="Whether this environment runs a prebuilt image reference or builds from an inline Dockerfile."
        >
          <select
            name="image_source"
            aria-label="Image source"
            value={values.imageSource}
            onChange={(e) => patch({ imageSource: parseImageSource(e.target.value) })}
            className={INPUT_CLASS}
          >
            <option value="image">Image reference</option>
            <option value="dockerfile">Dockerfile</option>
          </select>
        </Row>
        {values.imageSource === "image" ? (
          <Row
            title={<Label required>Image reference</Label>}
            help="Docker image or Daytona snapshot name (e.g. fabro-v11)."
          >
            <input
              type="text"
              name="docker_ref"
              aria-label="Image reference"
              value={values.dockerRef}
              onChange={(e) => patch({ dockerRef: e.target.value })}
              placeholder="ubuntu:24.04"
              autoComplete="off"
              spellCheck={false}
              className={`${INPUT_CLASS} font-mono`}
            />
          </Row>
        ) : (
          <Row
            title={<Label required>Dockerfile</Label>}
            help="Inline Dockerfile contents. The REST API accepts inline Dockerfiles only — local paths are rejected."
          >
            <textarea
              name="dockerfile"
              aria-label="Dockerfile"
              value={values.dockerfile}
              onChange={(e) => patch({ dockerfile: e.target.value })}
              rows={5}
              placeholder={"FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y git"}
              autoComplete="off"
              spellCheck={false}
              className={`${INPUT_CLASS} resize-y font-mono`}
            />
          </Row>
        )}
      </Panel>

      <Panel title="Resources">
        <Row title={<Label optional>CPU</Label>} help="Number of vCPUs allocated to each run. Leave blank for the provider default.">
          <input
            type="number"
            name="cpu"
            aria-label="CPU"
            step={1}
            value={values.cpu ?? ""}
            onChange={(e) => patch({ cpu: e.target.value === "" ? null : Number(e.target.value) })}
            placeholder="Provider default"
            className={`${INPUT_CLASS} font-mono`}
          />
        </Row>
        <Row title={<Label optional>Memory</Label>} help="Memory limit, including units (e.g. 512MiB or 4GB). Leave blank for the provider default.">
          <input
            type="text"
            name="memory"
            aria-label="Memory"
            value={values.memory}
            onChange={(e) => patch({ memory: e.target.value })}
            placeholder="Provider default"
            autoComplete="off"
            spellCheck={false}
            className={`${INPUT_CLASS} font-mono`}
          />
        </Row>
        <Row title={<Label optional>Disk</Label>} help="Disk limit, including units (e.g. 8GB). Leave blank for the provider default.">
          <input
            type="text"
            name="disk"
            aria-label="Disk"
            value={values.disk}
            onChange={(e) => patch({ disk: e.target.value })}
            placeholder="Provider default"
            autoComplete="off"
            spellCheck={false}
            className={`${INPUT_CLASS} font-mono`}
          />
        </Row>
      </Panel>

      <Panel title="Environment variables">
        <div className="px-4 py-3.5">
          <p className="mb-3 text-xs/5 text-fg-3">
            Variables injected into the sandbox for every run.
          </p>
          <KeyValueEditor
            entries={values.envVars}
            onChange={(envVars) => patch({ envVars })}
            keyPlaceholder="TZ"
            valuePlaceholder="UTC"
            addLabel="Add variable"
          />
        </div>
      </Panel>

      <Disclosure as="div" className="space-y-4" defaultOpen={hasNonDefaultAdvanced(values)}>
        <DisclosureButton className="group flex items-center gap-1.5 text-xs font-medium uppercase tracking-wider text-fg-muted transition-colors hover:text-fg-3 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500">
          <ChevronRightIcon
            className="size-3.5 transition-transform duration-150 group-data-open:rotate-90"
            aria-hidden="true"
          />
          Advanced
        </DisclosureButton>
        <DisclosurePanel className="space-y-6">
          <Panel title="Network">
            <Row
              title="Block all network access"
              help="Block all outbound network access from the sandbox."
            >
              <ToggleSwitch
                checked={values.blockNetwork}
                onChange={(blockNetwork) => patch({ blockNetwork })}
                label="Block all network access"
              />
            </Row>
          </Panel>

          <Panel title="Lifecycle">
            <Row title="Preserve" help="Keep the sandbox after the run finishes instead of tearing it down.">
              <ToggleSwitch
                checked={values.preserve}
                onChange={(preserve) => patch({ preserve })}
                label="Preserve sandbox after run"
              />
            </Row>
            <Row title="Stop on terminal" help="Stop the sandbox when the run reaches a terminal state.">
              <ToggleSwitch
                checked={values.stopOnTerminal}
                onChange={(stopOnTerminal) => patch({ stopOnTerminal })}
                label="Stop sandbox on terminal state"
              />
            </Row>
            <Row title={<Label optional>Auto-stop</Label>} help="Idle duration before the sandbox is stopped (e.g. 30m). Leave blank to disable.">
              <input
                type="text"
                name="auto_stop"
                aria-label="Auto-stop"
                value={values.autoStop}
                onChange={(e) => patch({ autoStop: e.target.value })}
                placeholder="30m"
                autoComplete="off"
                spellCheck={false}
                className={`${INPUT_CLASS} font-mono`}
              />
            </Row>
          </Panel>
        </DisclosurePanel>
      </Disclosure>

      {!lockId && values.id.trim() !== "" && !idValid ? (
        <p className="text-xs text-coral">
          ID must be lowercase letters, digits, or hyphens and start with a letter or digit.
        </p>
      ) : null}
    </>
  );
}

function ToggleSwitch({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: string;
}) {
  return (
    <Switch
      checked={checked}
      onChange={onChange}
      aria-label={label}
      className="group relative inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full bg-overlay-strong outline-1 -outline-offset-1 outline-line-strong transition-colors duration-150 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 data-checked:bg-teal-500"
    >
      <span className="pointer-events-none inline-block size-4 translate-x-0.5 rounded-full bg-fg shadow-sm transition-transform duration-150 group-data-checked:translate-x-[1.125rem]" />
    </Switch>
  );
}
