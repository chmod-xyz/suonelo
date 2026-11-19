import { Select as Primitive } from "@base-ui/react/select";
import { Check, ChevronDown } from "lucide-react";
import { useRef, useState, type ReactNode, type RefObject } from "react";
import { usePopupStagger } from "./use-popup-stagger";

function Options({ children, open }: { children: ReactNode; open: boolean }) {
  const list = useRef<HTMLDivElement>(null);
  usePopupStagger(list, '[role="option"]', open);
  return <Primitive.List ref={list} className="dropdown-list">{children}</Primitive.List>;
}

export function AnimatedSelect<T extends string | number>({
  label, items, value, change, disabled, portal, wide, compact,
}: {
  label: string; items: readonly { value: T; label: string }[]; value: T;
  change: (value: T) => void; disabled?: boolean;
  portal?: RefObject<HTMLElement | null>; wide?: boolean; compact?: boolean;
}) {
  const host = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  return <div ref={host} className={compact ? "search-sort" : "field"}>
    <Primitive.Root items={items} value={value} disabled={disabled}
      open={open} onOpenChange={setOpen}
      onValueChange={next => { if (next !== null) change(next); }}>
      {!compact && <Primitive.Label>{label}</Primitive.Label>}
      <Primitive.Trigger aria-label={compact ? label : undefined}
        className={compact ? "search-field dropdown-trigger" : "dropdown-trigger"}>
        <Primitive.Value />
        <Primitive.Icon render={<ChevronDown className="icon" aria-hidden />} />
      </Primitive.Trigger>
      <Primitive.Portal container={portal ?? host}>
        <Primitive.Positioner className="dropdown-position" data-wide={wide} align="end"
          sideOffset={5} collisionPadding={8} collisionBoundary={portal?.current ?? undefined} alignItemWithTrigger={false}>
          <Primitive.Popup className="dropdown-popup">
            <Options open={open}>
              {items.map(option => <Primitive.Item className="dropdown-item" key={option.value} value={option.value}>
                <Primitive.ItemText>{option.label}</Primitive.ItemText>
                <Primitive.ItemIndicator><Check className="icon" aria-hidden /></Primitive.ItemIndicator>
              </Primitive.Item>)}
            </Options>
          </Primitive.Popup>
        </Primitive.Positioner>
      </Primitive.Portal>
    </Primitive.Root>
  </div>;
}
