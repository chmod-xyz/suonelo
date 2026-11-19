import { invoke } from "@tauri-apps/api/core";
const stages = new Set<string>();
export function preloadStage(stage: string) {
  if (!(window as unknown as Record<string, unknown>).__SUONELO_PRELOAD_DIAGNOSTIC__) return;
  stages.add(stage);
  if (["queue", "bookmarks", "close"].every(name => stages.has(name))) {
    requestAnimationFrame(() => { void invoke("preload_frontend_ready").catch(() => undefined); });
  }
}
