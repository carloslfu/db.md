// SPDX-License-Identifier: Apache-2.0

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "todomvc-app-css/index.css";
import "./app.css";
import { App } from "./App";

const container = document.getElementById("root");
if (!container) throw new Error("missing #root");

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
