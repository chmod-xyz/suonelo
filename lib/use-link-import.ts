import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import { desktop, errorMessage, type LinkFileInspection } from "./desktop";

export function useLinkImport(
  dialog: RefObject<HTMLDialogElement | null>,
  input: RefObject<HTMLTextAreaElement | null>,
  disabled: boolean,
) {
  const [attachment, setAttachment] = useState<LinkFileInspection | null>(null);
  const [reading, setReading] = useState(false);
  const [hover, setHover] = useState(false);
  const [error, setError] = useState("");
  const [issues, setIssues] = useState<LinkFileInspection["invalid"]>([]);
  const [announcement, setAnnouncement] = useState("");
  const generation = useRef(0);
  const alive = useRef(true);
  const state = useRef({ disabled, reading, attached: Boolean(attachment) });
  state.current = { disabled, reading, attached: Boolean(attachment) };

  const inspect = useCallback(async (paths: string[]) => {
    setHover(false);
    if (state.current.disabled) return;
    if (state.current.attached) { setError("Remove the current attachment before adding another file."); return; }
    if (state.current.reading) { setError("A text file is already being checked."); return; }
    setIssues([]);
    if (paths.length !== 1) { setError("Choose one .txt file at a time."); return; }
    const request = ++generation.current;
    state.current.reading = true;
    setReading(true);
    setError("");
    setAnnouncement("Checking text file…");
    try {
      const result = await desktop.inspectLinkFile(paths[0]);
      if (!alive.current || request !== generation.current) return;
      if (!result.valid || result.content === null) {
        setError(result.reason || (result.invalidCount > 0
          ? `${result.invalidCount} invalid entries. Every entry must be a supported HTTPS Qobuz URL.`
          : "This file contains no Qobuz URLs."));
        setIssues(result.invalid);
        setAnnouncement("File rejected. Your text is unchanged.");
        return;
      }
      state.current.attached = true;
      setAttachment(result);
      setAnnouncement("Valid text file attached. Choose Add to queue or Bookmark when ready.");
    } catch (failure) {
      if (alive.current && request === generation.current) {
        setError(errorMessage(failure));
        setAnnouncement("File rejected. Your text is unchanged.");
      }
    } finally {
      if (alive.current && request === generation.current) {
        state.current.reading = false;
        setReading(false);
      }
    }
  }, []);

  const remove = useCallback(() => {
    generation.current++;
    state.current.attached = false;
    state.current.reading = false;
    setAttachment(null);
    setReading(false);
    setHover(false);
    setError("");
    setIssues([]);
    setAnnouncement("Attachment removed. Your previous text is restored.");
    requestAnimationFrame(() => { if (alive.current) input.current?.focus({ preventScroll: true }); });
  }, [input]);

  const choose = useCallback(async () => {
    if (state.current.disabled || state.current.reading || state.current.attached) return;
    const request = generation.current;
    try {
      const path = await open({ multiple: false, directory: false, filters: [{ name: "UTF-8 text files", extensions: ["txt"] }] });
      if (alive.current && request === generation.current && typeof path === "string") await inspect([path]);
    } catch (failure) {
      if (alive.current && request === generation.current) setError(errorMessage(failure));
    }
  }, [inspect]);

  useEffect(() => {
    alive.current = true;
    let dragSequence = 0;
    let unregister: Promise<() => void> | undefined;
    try {
      unregister = getCurrentWebview().onDragDropEvent(event => {
        const payload = event.payload;
        const sequence = ++dragSequence;
        const selection = generation.current;
        if (payload.type === "leave") { setHover(false); return; }
        void getCurrentWindow().scaleFactor().then(scale => {
          const element = dialog.current;
          const staleHover = payload.type !== "drop" && sequence !== dragSequence;
          if (!alive.current || selection !== generation.current || staleHover || !element?.open || !Number.isFinite(scale) || scale <= 0) return;
          const rect = element.getBoundingClientRect();
          const x = payload.position.x / scale;
          const y = payload.position.y / scale;
          const inside = x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
          setHover(inside && !state.current.attached && !state.current.reading && !state.current.disabled);
          if (payload.type === "drop") {
            setHover(false);
            if (inside) void inspect(payload.paths);
          }
        }).catch(() => {
          if (alive.current && payload.type === "drop") setError("File drop position is unavailable. Use Add TXT file.");
        });
      });
      void unregister.catch(() => undefined);
    } catch { /* Browser previews have no native drop surface. */ }
    const preventFileNavigation = (event: DragEvent) => {
      if (event.dataTransfer?.types.includes("Files")) event.preventDefault();
    };
    document.addEventListener("dragover", preventFileNavigation);
    document.addEventListener("drop", preventFileNavigation);
    return () => {
      alive.current = false;
      generation.current++;
      dragSequence++;
      void unregister?.then(unlisten => unlisten()).catch(() => undefined);
      document.removeEventListener("dragover", preventFileNavigation);
      document.removeEventListener("drop", preventFileNavigation);
    };
  }, [dialog, inspect]);

  function clearError() { setError(""); setIssues([]); setAnnouncement(""); }
  return { attachment, reading, hover, error, issues, announcement, choose, remove, clearError, generation, alive };
}
