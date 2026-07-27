// corpus/cases/rou3.ts — deterministic driver for rou3 (h3js/rou3 @ 39b0e9b).
// Imports only checked-in local source under corpus/projects/rou3/src.
// Exercises the runtime router surface (createRouter/addRoute/findRoute/
// removeRoute/routesOverlap/routeToRegExp) and prints stable, project-derived
// results. No hardcoded expected value; output is a function of the library.

import {
  createRouter,
  addRoute,
  findRoute,
  removeRoute,
  routesOverlap,
  routeToRegExp,
} from "../projects/rou3/src/index.ts";

function line(label: string, value: unknown): void {
  const s =
    typeof value === "string"
      ? value
      : value === undefined
        ? "<none>"
        : JSON.stringify(value);
  process.stdout.write(`${label}=${s}\n`);
}

// --- Router: static, param, wildcard, group, optional-modifier routes ---
const router = createRouter<string>();

addRoute(router, "GET", "/", "root");
addRoute(router, "GET", "/users", "users-index");
addRoute(router, "GET", "/users/:id", "user-by-id");
addRoute(router, "GET", "/users/:id/posts", "user-posts");
addRoute(router, "POST", "/users/:id", "create-user-sub");
addRoute(router, "GET", "/files/**:rest", "files-catchall");
addRoute(router, "GET", "/api/(v1|v2)/status", "api-status-group");
addRoute(router, "GET", "/items/:id?", "items-optional");

// --- Static + param lookups ---
line("static-root", findRoute(router, "GET", "/")?.data);
line("static-users", findRoute(router, "GET", "/users")?.data);
line("param-user", findRoute(router, "GET", "/users/42")?.data);
line("param-user-params", findRoute(router, "GET", "/users/42")?.params);
line("nested-posts", findRoute(router, "GET", "/users/42/posts")?.data);
line("nested-posts-params", findRoute(router, "GET", "/users/42/posts")?.params);
line("method-mismatch", findRoute(router, "PUT", "/users/42")?.data);

// --- Wildcard catch-all ---
line("wildcard", findRoute(router, "GET", "/files/a/b/c")?.data);
line("wildcard-params", findRoute(router, "GET", "/files/a/b/c")?.params);

// --- Group alternation ---
line("group-v1", findRoute(router, "GET", "/api/v1/status")?.data);
line("group-v2", findRoute(router, "GET", "/api/v2/status")?.data);
line("group-v3-miss", findRoute(router, "GET", "/api/v3/status")?.data);

// --- Optional modifier (:id?) matches both presence and absence ---
line("optional-present", findRoute(router, "GET", "/items/7")?.data);
line("optional-present-params", findRoute(router, "GET", "/items/7")?.params);
line("optional-absent", findRoute(router, "GET", "/items")?.data);

// --- Unknown route ---
line("unknown", findRoute(router, "GET", "/nope/42")?.data);

// --- Remove a route and confirm it no longer matches ---
removeRoute(router, "GET", "/users/:id");
line("after-remove", findRoute(router, "GET", "/users/42")?.data);
// Sibling unaffected
line("after-remove-sibling", findRoute(router, "GET", "/users/42/posts")?.data);

// --- Overlap detection (project-derived boolean + route info) ---
line(
  "overlap-static-param",
  routesOverlap("/users", "/users/:id") ? "true" : "false",
);
line(
  "overlap-param-disjoint",
  routesOverlap("/users/:id", "/posts/:id") ? "true" : "false",
);
line(
  "overlap-wildcard-nested",
  routesOverlap("/files/**:rest", "/files/x/y") ? "true" : "false",
);

// --- routeToRegExp: derive a RegExp from a pattern, exercise it ---
const rx = routeToRegExp("/users/:id");
line("regexp-source", rx.source);
line("regexp-flags", rx.flags);
const m = rx.exec("/users/99");
line("regexp-match-group1", m ? m[1] : "<none>");
