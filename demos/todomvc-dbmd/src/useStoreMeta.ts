// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";
import * as api from "./apiClient";
import type { ConnectionState } from "./apiClient";
import type { SchemaField } from "./types";

// The store declares its own schema in DB.md; the app reads it live and
// builds controls from it (every enum field becomes a picker). Refetched
// when DB.md itself changes — edit the schema and the UI follows.
export function useSchema(): SchemaField[] {
  const [fields, setFields] = useState<SchemaField[]>([]);
  useEffect(() => {
    const load = () => api.fetchSchema().then(setFields, () => {});
    load();
    return api.onStoreChange((paths) => {
      if (api.batchTouches(paths, (path) => path === "DB.md")) load();
    });
  }, []);
  return fields;
}

// Whether the store's API server is reachable (the SSE feed doubles as
// the liveness probe; EventSource reconnects on its own).
export function useConnection(): ConnectionState {
  const [state, setState] = useState<ConnectionState>("connecting");
  useEffect(() => api.onConnectionChange(setState), []);
  return state;
}
