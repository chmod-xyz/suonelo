import { LoaderCircle } from "lucide-react";
import * as ProgressPrimitive from "@radix-ui/react-progress";
import { Slot } from "@radix-ui/react-slot";
import { useId, type ComponentProps, type ReactNode } from "react";

export function Button({
  asChild = false,
  className,
  children,
  ...props
}: ComponentProps<"button"> & { asChild?: boolean }) {
  const Component = asChild ? Slot : "button";
  return <Component type="button" className={className} {...props}>{children}</Component>;
}

export function ValidationButton({ label, validating, className, ...props }: ComponentProps<"button"> & {
  label: string; validating: boolean;
}) {
  return (
    <Button {...props} className={["validation-button", className].filter(Boolean).join(" ")}
      data-label={label} aria-busy={validating} aria-label={validating ? "Validating…" : label}>
      <span>{validating ? <><LoaderCircle className="validation-spinner" aria-hidden />Validating…</> : label}</span>
    </Button>
  );
}

export function Field({ label, children, help }: { label: string; children: ReactNode; help?: ReactNode }) {
  return (
    <label className="field">
      {label}
      {children}
      {help && <small>{help}</small>}
    </label>
  );
}

export function SettingSwitch({ name, label, description, checked, change }: {
  name: string; label: string; description: string; checked: boolean; change: (checked: boolean) => void;
}) {
  const descriptionId = useId();
  return (
    <label className="switch-row">
      <span className="switch-copy">
        <span>{label}</span>
        <small id={descriptionId}>{description}</small>
      </span>
      <span className="switch-control">
        <input type="checkbox" role="switch" data-setting={name} checked={checked}
          aria-label={label} aria-describedby={descriptionId} onChange={event => change(event.target.checked)} />
        <span className="switch-track" aria-hidden />
      </span>
    </label>
  );
}

export function Progress({ value, label, active = true, paused }: { value: number | null; label: string; active?: boolean; paused?: boolean }) {
  const percent = value === null ? null : Math.min(100, Math.max(0, value));
  return (
    <ProgressPrimitive.Root value={percent} className="meter" data-active={active} data-paused={paused} aria-label={label}>
      <ProgressPrimitive.Indicator className="progress-fill" style={percent === null ? undefined : { width: `${percent}%` }} />
    </ProgressPrimitive.Root>
  );
}

export function interactiveTarget(target: EventTarget): boolean {
  return target instanceof Element && Boolean(target.closest("button,a,input,textarea,select,label"));
}
