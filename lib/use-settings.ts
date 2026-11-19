import { useCallback, useRef, useState } from "react";
import { desktop, errorMessage, type ConnectionLimit, type QueueSnapshot, type Settings, type SettingsPatch } from "./desktop";

function isConflict(error: unknown) {
  return Boolean(error && typeof error === "object" && "code" in error && error.code === "settings_conflict");
}

export function useSettings(onError: (message: string, type: "error") => void) {
  const [settings, setSettings] = useState<Settings>();
  const [saveStatus, setSaveStatus] = useState("Settings autosave after valid edits.");
  const committed = useRef<Settings>(null);
  const pending = useRef<SettingsPatch>({});
  const writing = useRef<Promise<void>>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const resetting = useRef<Promise<void>>(null);
  const legacyDark = useRef<boolean | null>(null);
  const [resetVersion, setResetVersion] = useState(0);

  const initialize = useCallback((initial: Settings, systemDark = false) => {
    legacyDark.current = initial.theme == null
      ? initial.appearance === "dark" || (initial.appearance === "system" && systemDark)
      : null;
    committed.current = initial;
    setSettings(initial);
  }, []);

  const flush = useCallback(async () => {
    clearTimeout(timer.current);
    if (resetting.current) return resetting.current;
    if (writing.current) return writing.current;
    const write = async () => {
      while (committed.current) {
        const patch = pending.current;
        const hasPatch = Object.keys(patch).length > 0;
        const conversion = !hasPatch && committed.current.theme == null ? legacyDark.current : null;
        if (!hasPatch && conversion === null) break;
        pending.current = {};
        try {
          const save = (current: Settings) => conversion === null
            ? desktop.patchSettings(current.revision, patch)
            : desktop.migrateTheme(current.revision, conversion);
          let saved: Settings;
          try {
            saved = await save(committed.current);
          } catch (error) {
            if (!isConflict(error)) throw error;
            const current = await desktop.readSettings();
            committed.current = current;
            saved = conversion !== null && current.theme != null ? current : await save(current);
          }
          if (conversion !== null && saved.theme == null) throw new Error("The shared theme could not be saved.");
          if (saved.theme != null) legacyDark.current = null;
          committed.current = saved;
          setSettings({ ...saved, ...pending.current });
          setSaveStatus("Settings saved.");
        } catch (error) {
          setSettings({ ...committed.current, ...pending.current });
          setSaveStatus("Settings could not be saved.");
          // Template validation is already shown beside the live preview.
          if (!error || typeof error !== "object" || !("code" in error) || error.code !== "output_template") {
            onError(errorMessage(error), "error");
          }
          if (!Object.keys(pending.current).length) throw error;
        }
      }
    };
    writing.current = write();
    try {
      await writing.current;
    } finally {
      writing.current = null;
    }
  }, [onError]);

  const patch = useCallback((changes: SettingsPatch) => {
    if (resetting.current) return;
    Object.assign(pending.current, changes);
    setSettings(current => current ? { ...current, ...changes } : current);
    setSaveStatus("Saving settings…");
    clearTimeout(timer.current);
    timer.current = setTimeout(() => void flush().catch(() => undefined), 330);
  }, [flush]);

  const reset = useCallback(async () => {
    if (resetting.current) return resetting.current;
    clearTimeout(timer.current);
    const previousPending = pending.current;
    pending.current = {};
    const restore = async () => {
      await writing.current?.catch(() => undefined);
      let revision = committed.current?.revision;
      if (revision === undefined) revision = (await desktop.readSettings()).revision;
      let defaults: Settings;
      try {
        defaults = await desktop.resetSettings(revision);
      } catch (error) {
        if (!isConflict(error)) throw error;
        const current = await desktop.readSettings();
        defaults = await desktop.resetSettings(current.revision);
      }
      initialize(defaults);
      setSaveStatus("Settings restored to defaults.");
      setResetVersion(previous => previous + 1);
    };
    resetting.current = restore();
    try {
      await resetting.current;
    } catch (error) {
      pending.current = previousPending;
      if (committed.current) setSettings({ ...committed.current, ...previousPending });
      const hasPending = Object.keys(previousPending).length > 0;
      setSaveStatus(hasPending ? "Saving settings…" : "Settings could not be reset.");
      if (hasPending) timer.current = setTimeout(() => void flush().catch(() => undefined), 330);
      throw error;
    } finally {
      resetting.current = null;
    }
  }, [flush, initialize]);

  const saveLimit = useCallback(async (limit: ConnectionLimit, acceptQueue: (queue: QueueSnapshot) => void) => {
    await flush();
    const commit = async () => {
      const current = committed.current ?? await desktop.readSettings();
      let result;
      try {
        result = await desktop.saveConnectionLimit(current.revision, limit);
      } catch (error) {
        if (!isConflict(error)) throw error;
        committed.current = await desktop.readSettings();
        result = await desktop.saveConnectionLimit(committed.current.revision, limit);
      }
      committed.current = result.settings;
      setSettings({ ...result.settings, ...pending.current });
      setSaveStatus("Settings saved.");
      acceptQueue(result.queue);
    };
    writing.current = commit();
    try {
      await writing.current;
    } finally {
      writing.current = null;
      if (Object.keys(pending.current).length) {
        clearTimeout(timer.current);
        timer.current = setTimeout(() => void flush().catch(() => undefined), 330);
      }
    }
  }, [flush]);

  const theme = settings?.theme ?? (legacyDark.current ? settings?.darkTheme : settings?.lightTheme) ?? "Suonelo";
  return { settings, theme, initialize, flush, patch, reset, resetVersion, saveStatus, saveLimit };
}
