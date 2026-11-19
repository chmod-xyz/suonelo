import { CircleAlert, CircleCheck, Info, TriangleAlert, X } from "lucide-react";

export type ToastType = "success" | "info" | "warning" | "error";
export type Notify = (message: string, type?: ToastType) => void;
export interface ToastMessage { message: string; type: ToastType }

const icons = { success: CircleCheck, info: Info, warning: TriangleAlert, error: CircleAlert };

export function Toast({ toast, dismiss }: { toast: ToastMessage | null; dismiss: () => void }) {
  const Icon = toast ? icons[toast.type] : Info;
  return (
    <div className="toast" data-type={toast?.type} role={toast?.type === "error" ? "alert" : "status"} aria-atomic="true">
      {toast && <>
        <Icon className="icon" aria-hidden />
        <span><span className="sr-only">{toast.type}: </span>{toast.message}</span>
        <button className="ghost" aria-label="Dismiss notification" onClick={dismiss}><X className="icon" aria-hidden /></button>
      </>}
    </div>
  );
}
