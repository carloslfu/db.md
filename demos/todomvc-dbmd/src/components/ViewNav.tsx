// SPDX-License-Identifier: Apache-2.0

import type { View } from "../types";

// One store, several views: the classic list is just the first lens. The
// nav borrows the filter pills' visual language, one level up.
const VIEWS: { value: View; label: string; href: string }[] = [
  { value: "todos", label: "todos", href: "#/" },
  { value: "search", label: "search", href: "#/search" },
  { value: "checklist", label: "checklist", href: "#/checklist" },
  { value: "activity", label: "activity", href: "#/activity" },
  { value: "ask", label: "ask", href: "#/ask" },
  { value: "store", label: "store", href: "#/store" },
];

export function ViewNav({ view }: { view: View }) {
  return (
    <nav className="view-nav" aria-label="Store views">
      <ul>
        {VIEWS.map(({ value, label, href }) => (
          <li key={value}>
            <a className={view === value ? "selected" : ""} href={href}>
              {label}
            </a>
          </li>
        ))}
      </ul>
    </nav>
  );
}
