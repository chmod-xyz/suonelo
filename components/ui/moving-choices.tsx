import { useLayoutEffect, useRef, type ComponentProps } from "react";

export function MovingChoices({ value, className, children, ref, ...group }: ComponentProps<"div"> & {
  value: string | number;
}) {
  const container = useRef<HTMLDivElement>(null);
  const highlight = useRef<HTMLSpanElement>(null);
  useLayoutEffect(() => {
    const element = container.current;
    const pill = highlight.current;
    const selected = element?.querySelector<HTMLElement>(`[data-choice="${value}"]`);
    if (!element || !pill || !selected) return;
    const measure = () => {
      pill.style.transform = `translate3d(${selected.offsetLeft}px,${selected.offsetTop}px,0)`;
      pill.style.width = `${selected.offsetWidth}px`;
      pill.style.height = `${selected.offsetHeight}px`;
      pill.dataset.ready = "true";
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    observer.observe(selected);
    return () => observer.disconnect();
  }, [value]);
  return <div ref={ref} {...group}>
    <div ref={container} className={className}>
      <span className="choice-highlight" ref={highlight} aria-hidden />
      {children}
    </div>
  </div>;
}
