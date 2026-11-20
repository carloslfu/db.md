// SPDX-License-Identifier: Apache-2.0

import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { useEffect, useRef, useState } from "react";
import type { FocusEvent, KeyboardEvent } from "react";
import type { TodoLinks } from "../derive";
import type { SchemaField, Todo } from "../types";
import type { TodoAction } from "../useTodos";
import { BodyEditor } from "./BodyEditor";

// One dot color per enum position; wraps for longer enums. For the stock
// priority field this lands on sage / amber / rose.
const ENUM_PALETTE = ["#87a06f", "#c9a227", "#b83f45", "#6a7fb5", "#8f6ab5"];

export function TodoItem({
  todo,
  dispatch,
  persistDescription,
  enumFields,
  links,
  revealTick,
  onReveal,
}: {
  todo: Todo;
  dispatch: (action: TodoAction) => void;
  persistDescription: (id: string, description: string) => void;
  enumFields: SchemaField[];
  links?: TodoLinks;
  revealTick?: number;
  onReveal: (id: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [flashing, setFlashing] = useState(false);
  const editRef = useRef<HTMLInputElement>(null);
  const liRef = useRef<HTMLLIElement | null>(null);
  const skipBlur = useRef(false);

  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: todo.id,
  });

  useEffect(() => {
    if (!editing) return;
    const input = editRef.current;
    if (!input) return;
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
  }, [editing]);

  // Another view pointed here (a search hit, a checklist group, a log row,
  // a link chip): scroll to the row, open its description, flash once.
  useEffect(() => {
    if (revealTick === undefined) return;
    liRef.current?.scrollIntoView({ block: "center", behavior: "smooth" });
    if ((todo.description ?? "") !== "") setExpanded(true);
    setFlashing(true);
    const timer = window.setTimeout(() => setFlashing(false), 1500);
    return () => window.clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [revealTick]);

  const commit = (value: string) => {
    const title = value.trim();
    if (!title) dispatch({ type: "REMOVE_ITEM", id: todo.id });
    else if (title !== todo.title) dispatch({ type: "UPDATE_ITEM", id: todo.id, title });
    setEditing(false);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      skipBlur.current = true;
      commit(event.currentTarget.value);
    } else if (event.key === "Escape") {
      skipBlur.current = true;
      setEditing(false);
    }
  };

  const onBlur = (event: FocusEvent<HTMLInputElement>) => {
    if (skipBlur.current) {
      skipBlur.current = false;
      return;
    }
    commit(event.currentTarget.value);
  };

  const hasDescription = (todo.description ?? "") !== "";
  const className = [
    todo.completed && "completed",
    editing && "editing",
    isDragging && "dragging",
    flashing && "revealed",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <li
      ref={(node) => {
        setNodeRef(node);
        liRef.current = node;
      }}
      className={className || undefined}
      style={{ transform: CSS.Transform.toString(transform), transition }}
    >
      <div className="view">
        <span
          className="drag-handle"
          {...attributes}
          {...listeners}
          aria-label={`Reorder ${todo.title}`}
        >
          ⠿
        </span>
        <input
          className="toggle"
          type="checkbox"
          checked={todo.completed}
          onChange={() => dispatch({ type: "TOGGLE_ITEM", id: todo.id })}
          aria-label={`Toggle ${todo.title}`}
        />
        <label onDoubleClick={() => setEditing(true)}>{todo.title}</label>
        {/* Schema-declared enum fields (everything but `status`, which the
            checkbox owns) render as cycling dots — the store's contract
            drives the control, not app code. */}
        {enumFields.map((field, index) => {
          const values = field.enum ?? [];
          const current = String(todo.fm?.[field.name] ?? "");
          const at = values.indexOf(current);
          const next = values[(at + 1) % values.length] ?? values[0];
          if (next === undefined) return null;
          return (
            <button
              key={field.name}
              className={`fm-badge${at >= 0 ? " set" : ""}`}
              style={{
                right: `${100 + index * 22}px`,
                color: at >= 0 ? ENUM_PALETTE[at % ENUM_PALETTE.length] : undefined,
              }}
              onClick={() =>
                dispatch({ type: "SET_FIELD", id: todo.id, key: field.name, value: next })
              }
              title={`${field.name}: ${at >= 0 ? current : "unset"} — click to cycle`}
              aria-label={`${field.name} of ${todo.title}: ${at >= 0 ? current : "unset"}. Set to ${next}.`}
            />
          );
        })}
        <button
          className={[
            "details-toggle",
            hasDescription && "has-details",
            expanded && "open",
          ]
            .filter(Boolean)
            .join(" ")}
          onClick={() => setExpanded((value) => !value)}
          aria-expanded={expanded}
          aria-label={`${expanded ? "Hide" : "Show"} description for ${todo.title}`}
        >
          ❯
        </button>
        <button
          className="destroy"
          onClick={() => dispatch({ type: "REMOVE_ITEM", id: todo.id })}
          aria-label={`Delete ${todo.title}`}
        />
      </div>
      {editing && (
        <input
          ref={editRef}
          className="edit"
          defaultValue={todo.title}
          onKeyDown={onKeyDown}
          onBlur={onBlur}
          aria-label={`Edit ${todo.title}`}
        />
      )}
      {expanded && !editing && (
        <div className="details">
          <BodyEditor
            value={todo.description ?? ""}
            onSave={(markdown) => persistDescription(todo.id, markdown)}
          />
          {links && (links.out.length > 0 || links.in.length > 0) && (
            <div className="details-links">
              {links.out.length > 0 && (
                <span className="links-group">
                  links to
                  {links.out.map((other) => (
                    <button
                      key={other.id}
                      className="link-chip"
                      onClick={() => onReveal(other.id)}
                    >
                      {other.title}
                    </button>
                  ))}
                </span>
              )}
              {links.in.length > 0 && (
                <span className="links-group">
                  linked from
                  {links.in.map((other) => (
                    <button
                      key={other.id}
                      className="link-chip"
                      onClick={() => onReveal(other.id)}
                    >
                      {other.title}
                    </button>
                  ))}
                </span>
              )}
            </div>
          )}
          <div className="details-path">
            {todo.path ? `db/${todo.path}` : ""} · wysiwyg markdown, saved to the record body
          </div>
        </div>
      )}
    </li>
  );
}
