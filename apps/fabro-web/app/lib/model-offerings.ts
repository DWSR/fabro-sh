import type { Model, Provider } from "@qltysh/fabro-api-client";

type ModelOfferingIdentity = Pick<Model, "provider" | "id">;

export function modelOfferingKey(model: ModelOfferingIdentity): string {
  return `${model.provider}\u0000${model.id}`;
}

/** Provider ids to the names the catalog displays, falling back to the id. */
export function providerNamesById(
  providers: readonly Pick<Provider, "id" | "display_name">[],
): Map<string, string> {
  return new Map(providers.map((p) => [p.id, p.display_name || p.id]));
}

export function modelOfferingTestArgs(
  model: ModelOfferingIdentity,
): [id: string, provider: string] {
  return [model.id, model.provider];
}
