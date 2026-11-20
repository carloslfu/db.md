// SPDX-License-Identifier: Apache-2.0

import { Crepe } from "@milkdown/crepe";
import { remarkStringifyOptionsCtx } from "@milkdown/kit/core";
import { replaceAll } from "@milkdown/kit/utils";
import { useEffect, useRef } from "react";
import "@milkdown/crepe/theme/common/style.css";
import "@milkdown/crepe/theme/classic.css";

// WYSIWYG surface for the record body. Milkdown/Crepe is markdown-native
// (ProseMirror + remark), so what the editor holds is isomorphic to the
// file's markdown: checkbox clicks, block drags, and typing all funnel into
// one markdownUpdated stream that we debounce into saves. External changes
// (an agent writing the record, arriving over SSE) are applied with
// replaceAll unless the user is focused in the editor.
const trimTail = (markdown: string) => markdown.replace(/\s+$/, "");

export function BodyEditor({
  value,
  onSave,
}: {
  value: string;
  onSave: (markdown: string) => void;
}) {
  const rootRef = useRef<HTMLDivElement>(null);
  const crepeRef = useRef<Crepe | null>(null);
  const readyRef = useRef(false);
  const focusedRef = useRef(false);
  const lastEmitted = useRef(value); // what the editor holds now, as markdown
  const loadedRef = useRef(value); // what was last loaded INTO the editor
  const valueRef = useRef(value);
  const saveTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const onSaveRef = useRef(onSave);
  onSaveRef.current = onSave;

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;

    // Dirty means the editor diverged from what it was seeded with — never
    // compare against the live prop, or an external change arriving while
    // focused would make a plain blur "save" stale content over it.
    const flush = () => {
      clearTimeout(saveTimer.current);
      const markdown = trimTail(lastEmitted.current);
      if (markdown !== trimTail(loadedRef.current)) {
        loadedRef.current = lastEmitted.current;
        onSaveRef.current(markdown);
      }
    };

    const crepe = new Crepe({
      root,
      defaultValue: valueRef.current,
      features: {
        [Crepe.Feature.Latex]: false,
        [Crepe.Feature.ImageBlock]: false,
      },
      featureConfigs: {
        [Crepe.Feature.Placeholder]: {
          text: "Add a description — markdown, stored in the record body",
        },
      },
    });
    // Serialize lists with "-" bullets — the store's own convention — so
    // editor saves don't churn agent-written "- [ ]" lines into "* [ ]".
    crepe.editor.config((ctx) => {
      ctx.update(remarkStringifyOptionsCtx, (options) => ({ ...options, bullet: "-" as const }));
    });
    crepe.on((listener) => {
      listener.markdownUpdated((_ctx, markdown) => {
        lastEmitted.current = markdown;
        clearTimeout(saveTimer.current);
        saveTimer.current = setTimeout(flush, 600);
      });
    });
    void crepe.create().then(() => {
      readyRef.current = true;
    });
    crepeRef.current = crepe;

    // Focus tracking via DOM events — Milkdown's own focus/blur listeners
    // proved unreliable across clicks into toolbars and gutters.
    const applyDeferredExternal = () => {
      const crepeNow = crepeRef.current;
      if (
        crepeNow &&
        readyRef.current &&
        trimTail(valueRef.current) !== trimTail(lastEmitted.current)
      ) {
        lastEmitted.current = valueRef.current;
        loadedRef.current = valueRef.current;
        crepeNow.editor.action(replaceAll(valueRef.current, true));
      }
    };
    const onFocusIn = () => {
      focusedRef.current = true;
    };
    const onFocusOut = (event: FocusEvent) => {
      if (root.contains(event.relatedTarget as Node | null)) return;
      focusedRef.current = false;
      flush();
      // An external update that arrived while focused was deferred;
      // apply it now that the user's edits (if any) are flushed.
      applyDeferredExternal();
    };
    root.addEventListener("focusin", onFocusIn);
    root.addEventListener("focusout", onFocusOut);

    return () => {
      clearTimeout(saveTimer.current);
      flush();
      root.removeEventListener("focusin", onFocusIn);
      root.removeEventListener("focusout", onFocusOut);
      readyRef.current = false;
      crepeRef.current = null;
      void crepe.destroy();
    };
  }, []);

  // External updates: follow the store unless the user is typing right now.
  useEffect(() => {
    valueRef.current = value;
    const crepe = crepeRef.current;
    if (!crepe || !readyRef.current || focusedRef.current) return;
    if (trimTail(value) === trimTail(lastEmitted.current)) return;
    lastEmitted.current = value;
    loadedRef.current = value;
    crepe.editor.action(replaceAll(value, true));
  }, [value]);

  return <div ref={rootRef} className="details-crepe" />;
}
