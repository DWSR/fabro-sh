import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { createMemoryRouter, RouterProvider, type RouteObject } from "react-router";

import { installRoutes } from "./install-router";
import { routes } from "./router";

async function renderPage(routeObjects: RouteObject[], path: string, navigate = false) {
  const router = createMemoryRouter(routeObjects, {
    initialEntries: [navigate ? "/" : path],
  });
  try {
    if (navigate) await router.navigate(path);
    return renderToStaticMarkup(<RouterProvider router={router} />);
  } finally {
    router.dispose();
  }
}

describe("root error boundary", () => {
  for (const path of ["/some_random_route", "/runs/existing-run/billing"]) {
    for (const navigate of [false, true]) {
      test(`${path} displays 404 on ${navigate ? "navigation" : "initial load"}`, async () => {
        const html = await renderPage(routes, path, navigate);

        expect(html).toContain(">404</p>");
        expect(html).toContain("Page not found");
        expect(html).not.toContain("Something went wrong");
        expect(html).toContain('href="/runs"');
        expect(html).toContain("Go back");
      });
    }
  }

  for (const [name, root] of [["app", routes[0]], ["installer", installRoutes[0]]] as const) {
    async function renderFailure(error: unknown) {
      return renderPage([
        {
          ...root,
          children: [{
            path: "failure",
            loader() { throw error; },
            Component: () => null,
          }],
        },
      ], "/failure", true);
    }

    test(`${name} displays a route's 404 response as page not found`, async () => {
      const html = await renderFailure(new Response(null, { status: 404 }));

      expect(html).toContain(">404</p>");
      expect(html).toContain("Page not found");
      expect(html).not.toContain("Something went wrong");
    });

    test.each([403, 500])(`${name} preserves a route's %i error response`, async (status) => {
      const statusText = status === 403 ? "Forbidden" : "Internal Server Error";
      const html = await renderFailure(new Response(null, { status, statusText }));

      expect(html).toContain(`>${status}</p>`);
      expect(html).toContain(`${status} Error`);
      expect(html).toContain(statusText);
      expect(html).not.toContain("Page not found");
    });

    test(`${name} keeps unexpected exceptions as 500 errors`, async () => {
      const html = await renderFailure(new Error("Unexpected route failure"));

      expect(html).toContain(">500</p>");
      expect(html).toContain("Something went wrong");
      expect(html).toContain("Unexpected route failure");
      expect(html).not.toContain("Page not found");
    });
  }
});
