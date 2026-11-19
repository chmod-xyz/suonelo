import { useLayoutEffect, useRef } from "react";

function Digit({ digit, direction }: { digit: string; direction: 1 | -1 }) {
  const current = useRef<HTMLSpanElement>(null);
  const outgoing = useRef<HTMLSpanElement>(null);
  const previous = useRef(digit);
  useLayoutEffect(() => {
    const from = previous.current;
    previous.current = digit;
    const incoming = current.current!;
    const old = outgoing.current!;
    if (from === digit || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const frame = getComputedStyle(incoming);
    const offset = frame.transform;
    const opacity = frame.opacity;
    incoming.getAnimations().forEach(animation => animation.cancel());
    old.getAnimations().forEach(animation => animation.cancel());
    old.textContent = from;
    const options = { duration: 240, easing: "cubic-bezier(.215,.61,.355,1)" };
    old.animate([{ transform: offset, opacity }, { transform: `translateY(${-direction * 100}%)`, opacity: 0 }], options);
    incoming.animate([{ transform: `translateY(${direction * 100}%)`, opacity: 0 }, { transform: "translateY(0)", opacity: 1 }], options);
  }, [digit, direction]);
  return <span className="rolling-digit" data-empty={digit === " " ? "" : undefined}>
    <span ref={outgoing} className="rolling-outgoing" />
    <span ref={current} className="rolling-current">{digit}</span>
  </span>;
}

export function RollingPercent({ value }: { value: number }) {
  const percent = Math.floor(value);
  const previous = useRef(percent);
  const direction = percent < previous.current ? -1 : 1;
  useLayoutEffect(() => { previous.current = percent; }, [percent]);
  const digits = String(percent).padStart(3, " ").split("");
  return <span className="rolling-percent">
    <span className="sr-only">{percent}%</span>
    <span aria-hidden>{digits.map((digit, index) => <Digit key={index} digit={digit} direction={direction} />)}%</span>
  </span>;
}
