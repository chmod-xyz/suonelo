import { useEffect, useRef, type RefObject } from "react";

export function usePopupStagger(list: RefObject<HTMLElement | null>, selector: string, open: boolean) {
  const lastClose = useRef(-Infinity);
  const played = useRef(false);
  useEffect(() => {
    if (!open) {
      lastClose.current = performance.now();
      return;
    }
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    if (played.current && performance.now() - lastClose.current < 220) return;
    const frame = requestAnimationFrame(() => {
      const element = list.current;
      if (!element) return;
      played.current = true;
      const bounds = element.getBoundingClientRect();
      [...element.querySelectorAll(selector)].filter(row => {
        const { top, bottom } = row.getBoundingClientRect();
        return top < bounds.bottom && bottom > bounds.top;
      }).forEach((row, index) => row.animate([
        { opacity: 0, translate: "8px 0", filter: "blur(4px)" },
        { opacity: 1, translate: "0 0", filter: "blur(0)" },
      ], { duration: 300, delay: Math.min(index * 16, 200), easing: "cubic-bezier(.215,.61,.355,1)", fill: "backwards" }));
    });
    return () => cancelAnimationFrame(frame);
  }, [list, selector, open]);
}
