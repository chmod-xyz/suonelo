import type { CSSProperties } from "react";

export function ShellIcon({ name }: { name: "exit" | "downloads" | "bookmarks" | "settings" }) {
  return <span className="icon shell-icon" aria-hidden="true" style={{ maskImage: `url(/assets/shell/${name}.svg)`, WebkitMaskImage: `url(/assets/shell/${name}.svg)` } as CSSProperties} />;
}
