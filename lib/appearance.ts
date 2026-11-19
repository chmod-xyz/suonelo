import { flushSync } from "react-dom";

let transition: ViewTransition | undefined;
let sequence = 0;

export function animateAppearance(update: () => void) {
  const current = ++sequence;
  transition?.skipTransition();
  const apply = () => { if (current === sequence) flushSync(update); };
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches || !document.startViewTransition) { apply(); return; }
  try {
    transition = document.startViewTransition(apply);
    void transition.ready.catch(() => undefined);
    void transition.finished.catch(() => undefined);
  } catch { apply(); }
}
