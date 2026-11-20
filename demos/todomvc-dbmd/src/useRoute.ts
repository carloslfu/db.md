// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";
import type { Filter, View } from "./types";

// TodoMVC's classic hash routing, extended one level: the store has
// several views, and the classic filters live inside the todos view.
//   #/ · #/active · #/completed        → the todo list
//   #/search · #/checklist · #/activity · #/ask · #/store → the other views
export type Route = { view: View; filter: Filter };

function parseHash(): Route {
  switch (window.location.hash) {
    case "#/active":
      return { view: "todos", filter: "active" };
    case "#/completed":
      return { view: "todos", filter: "completed" };
    case "#/search":
      return { view: "search", filter: "all" };
    case "#/checklist":
      return { view: "checklist", filter: "all" };
    case "#/activity":
      return { view: "activity", filter: "all" };
    case "#/ask":
      return { view: "ask", filter: "all" };
    case "#/store":
      return { view: "store", filter: "all" };
    default:
      return { view: "todos", filter: "all" };
  }
}

export function useRoute(): Route {
  const [route, setRoute] = useState<Route>(parseHash);
  useEffect(() => {
    const onHashChange = () => setRoute(parseHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);
  return route;
}
