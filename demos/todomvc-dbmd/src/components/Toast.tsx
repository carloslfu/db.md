// SPDX-License-Identifier: Apache-2.0

export type ToastState = {
  message: string;
  /** Optional smaller second line (e.g. the server's suggestion). */
  detail?: string;
  /** With actions the toast persists until one is clicked. */
  actions?: { label: string; danger?: boolean; onClick: () => void }[];
};

// One quiet notice at a time — op failures, and the RM_LINKED confirm
// (the store refusing to delete a still-linked record).
export function Toast({ toast, onDismiss }: { toast: ToastState | null; onDismiss: () => void }) {
  if (!toast) return null;
  return (
    <div className="toast" role="alert">
      <div className="toast-message">{toast.message}</div>
      {toast.detail && <div className="toast-detail">{toast.detail}</div>}
      <div className="toast-actions">
        {toast.actions?.map(({ label, danger, onClick }) => (
          <button key={label} className={danger ? "danger" : ""} onClick={onClick}>
            {label}
          </button>
        ))}
        <button onClick={onDismiss}>{toast.actions ? "keep it" : "dismiss"}</button>
      </div>
    </div>
  );
}
