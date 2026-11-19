import { useLayoutEffect, useRef, useState, type ReactNode } from "react";

export function ReleaseDetails({ id, open, children }: { id: string; open: boolean; children: ReactNode }) {
  const panel = useRef<HTMLDivElement>(null);
  const animation = useRef<Animation>(null);
  const [present, setPresent] = useState(open);
  const previous = useRef(open);

  useLayoutEffect(() => {
    const element = panel.current;
    if (!element) return;
    const settle = () => {
      const current = animation.current;
      animation.current = null;
      current?.cancel();
      element.style.height = "";
      delete element.dataset.moving;
    };
    if (!open && !present) {
      settle();
      return;
    }

    const content = element.firstElementChild;
    if (!content || previous.current === open) return;
    previous.current = open;
    if (open) setPresent(true);

    const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
    const frame = getComputedStyle(element);
    const from = { height: frame.height, opacity: frame.opacity, filter: frame.filter };
    animation.current?.cancel();
    element.dataset.moving = "true";
    const targetHeight = open ? `${content.getBoundingClientRect().height}px` : "0px";
    if (reduced) element.style.height = targetHeight;
    const motion = element.animate([
      { height: reduced ? undefined : from.height, opacity: from.opacity, filter: from.filter },
      { height: reduced ? undefined : targetHeight, opacity: open ? 1 : 0, filter: open ? "blur(0)" : "blur(2px)" },
    ], { duration: 350, easing: "cubic-bezier(.215,.61,.355,1)", fill: "both" });
    animation.current = motion;
    motion.onfinish = () => {
      if (animation.current !== motion) return;
      if (open) settle();
      // Keep the final frame until React has hidden the closing panel.
      else setPresent(false);
    };
  }, [open, present]);

  useLayoutEffect(() => () => animation.current?.cancel(), []);

  return <div id={id} ref={panel} className="release-details" data-open={open} data-collapsed={!present}
    hidden={!present && !open} inert={!open}>
    {(present || open) && <div className="details">{children}</div>}
  </div>;
}
