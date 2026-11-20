// SPDX-License-Identifier: Apache-2.0

import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import type { DragEndEvent } from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  sortableKeyboardCoordinates,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import type { TodoLinks } from "../derive";
import type { Filter, SchemaField, Todo } from "../types";
import type { TodoAction } from "../useTodos";
import { TodoItem } from "./TodoItem";

export function Main({
  todos,
  filter,
  dispatch,
  persistDescription,
  enumFields,
  linkIndex,
  reveal,
  onReveal,
}: {
  todos: Todo[];
  filter: Filter;
  dispatch: (action: TodoAction) => void;
  persistDescription: (id: string, description: string) => void;
  enumFields: SchemaField[];
  linkIndex: Map<string, TodoLinks>;
  reveal: { id: string; nonce: number } | null;
  onReveal: (id: string) => void;
}) {
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  if (todos.length === 0) return null;

  const visible = todos.filter((todo) =>
    filter === "active" ? !todo.completed : filter === "completed" ? todo.completed : true,
  );
  const allCompleted = todos.every((todo) => todo.completed);

  const onDragEnd = (event: DragEndEvent) => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const oldIndex = visible.findIndex((todo) => todo.id === active.id);
    const newIndex = visible.findIndex((todo) => todo.id === over.id);
    if (oldIndex < 0 || newIndex < 0) return;
    // Neighbors are taken from the post-move *visible* list, so reordering
    // works under the Active/Completed filters too — the server ranks the
    // moved record between those two records in the full list.
    const moved = arrayMove(visible, oldIndex, newIndex);
    const position = moved.findIndex((todo) => todo.id === active.id);
    dispatch({
      type: "REORDER_ITEM",
      id: String(active.id),
      prevId: moved[position - 1]?.id,
      nextId: moved[position + 1]?.id,
    });
  };

  return (
    <main className="main">
      <div className="toggle-all-container">
        <input
          id="toggle-all"
          className="toggle-all"
          type="checkbox"
          checked={allCompleted}
          onChange={(event) => dispatch({ type: "TOGGLE_ALL", completed: event.target.checked })}
        />
        <label className="toggle-all-label" htmlFor="toggle-all">
          Toggle All Input
        </label>
      </div>
      <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
        <SortableContext
          items={visible.map((todo) => todo.id)}
          strategy={verticalListSortingStrategy}
        >
          <ul className="todo-list">
            {visible.map((todo) => (
              <TodoItem
                key={todo.id}
                todo={todo}
                dispatch={dispatch}
                persistDescription={persistDescription}
                enumFields={enumFields}
                links={linkIndex.get(todo.id)}
                revealTick={reveal !== null && reveal.id === todo.id ? reveal.nonce : undefined}
                onReveal={onReveal}
              />
            ))}
          </ul>
        </SortableContext>
      </DndContext>
    </main>
  );
}
