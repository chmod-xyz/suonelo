import { useId, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { Button } from "./ui/primitives";
import { MovingChoices } from "./ui/moving-choices";

const choices = [
  { tier: 1, label: "Lossy", file: "lossy", width: 47.5335, palette: "dark" },
  { tier: 2, label: "CD", file: "cd", width: 24, palette: "light" },
  { tier: 3, label: "Studio", file: "studio", width: 24.75, palette: "dark" },
  { tier: 4, label: "Hi-Res", file: "hi-res", width: 24, palette: "dark" },
];

function colorChannels(container: HTMLElement, token: string, context: CanvasRenderingContext2D) {
  const probe = document.createElement("span");
  probe.style.color = `var(${token})`;
  probe.style.position = "absolute";
  container.append(probe);
  context.fillStyle = getComputedStyle(probe).color;
  probe.remove();
  context.fillRect(0, 0, 1, 1);
  return [...context.getImageData(0, 0, 1, 1).data].slice(0, 3).map(channel => channel / 255);
}

export function QualityPicker({ value, change, allowFallback, previewFallback, mode, disabled }: {
  value: number; change: (quality: number) => void; allowFallback: boolean; previewFallback?: boolean; mode: string; disabled?: boolean;
}) {
  const heading = useId();
  const filterId = useId().replaceAll(":", "");
  const group = useRef<HTMLDivElement>(null);
  const [filters, setFilters] = useState<Record<string, string>>({});

  useLayoutEffect(() => {
    const element = group.current;
    const context = document.createElement("canvas").getContext("2d");
    if (!element || !context) return;
    const ink = colorChannels(element, "--ink", context);
    const surface = colorChannels(element, "--surface", context);
    const on = colorChannels(element, "--on", context);
    const accent = colorChannels(element, "--accent", context);
    const darkLuminance = (12 * .2126 + 9 * .7152 + 12 * .0722) / 255;
    const result: Record<string, string> = {};
    for (const [name, foreground, background, light] of [
      ["dark-normal", ink, surface, false], ["dark-selected", on, accent, false],
      ["light-normal", ink, surface, true], ["light-selected", on, accent, true],
    ] as const) {
      const rows = foreground.flatMap((channel, index) => {
        if (light) return [0, 0, 0, 0, channel];
        const slope = (background[index] - channel) / (1 - darkLuminance);
        return [.2126 * slope, .7152 * slope, .0722 * slope, 0, channel - slope * darkLuminance];
      });
      result[name] = [...rows, 0, 0, 0, 1, 0].join(" ");
    }
    setFilters(result);
  }, [mode]);

  function keydown(event: KeyboardEvent<HTMLButtonElement>, tier: number) {
    const offset = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
    const next = event.key === "Home" ? 1 : event.key === "End" ? 4 : offset ? (tier - 1 + offset + 4) % 4 + 1 : null;
    if (next === null) return;
    event.preventDefault();
    change(next);
    group.current?.querySelector<HTMLButtonElement>(`[data-quality="${next}"]`)?.focus();
  }

  return (
    <section className="dialog-quality" aria-labelledby={heading}>
      <h3 className="quality-title" id={heading}>Quality</h3>
      <p className="quality-note">
        {allowFallback ? "Target quality is not guaranteed; fallback is currently enabled." : previewFallback ? "Only the requested full-track quality will be accepted." : "Only the requested quality will be accepted."}
        {previewFallback && " If only a preview is available, it will be saved as a labeled MP3."}
      </p>
      <svg className="quality-filter-defs" aria-hidden width="0" height="0">
        <defs>{Object.entries(filters).map(([name, values]) => (
          <filter key={name} id={`${filterId}-${name}`} colorInterpolationFilters="sRGB">
            <feColorMatrix type="matrix" values={values} />
          </filter>
        ))}</defs>
      </svg>
      <MovingChoices ref={group} className="quality-options" role="radiogroup" aria-labelledby={heading} value={value}>
        {choices.map(choice => (
          <Button key={choice.tier} role="radio" data-choice={choice.tier} aria-checked={value === choice.tier} data-quality={choice.tier} disabled={disabled}
            tabIndex={value === choice.tier ? 0 : -1} onClick={() => change(choice.tier)}
            onKeyDown={event => keydown(event, choice.tier)}>
            <span className="quality-icon-slot">
              <img className="quality-asset" src={`/assets/quality/${choice.file}.svg`} alt=""
                width={choice.width} height={24}
                style={{ filter: `url(#${filterId}-${choice.palette}-${value === choice.tier ? "selected" : "normal"})` }} />
            </span>
            <span>{choice.label}</span>
          </Button>
        ))}
      </MovingChoices>
    </section>
  );
}
