"use client";

import { preloadStage } from "@/lib/preload-diagnostic";
import { listen } from "@tauri-apps/api/event";
import { Bookmark as BookmarkIcon, CirclePause, Cog, Download, Play, Search, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { AccountSetupDialog, AddLinks, Confirmation } from "@/components/dialogs";
import { CatalogSearch } from "@/components/catalog-search";
import { SpeedControl } from "@/components/connection-controls";
import { AddLinkButton, Bookmarks, Downloads, filters, matches, type Filter } from "@/components/library";
import { accountLabels, SettingsPage, type Section } from "@/components/settings";
import { Button, ValidationButton } from "@/components/ui/primitives";
import { Toast, type ToastMessage, type ToastType } from "@/components/toast";
import { SuoneloLogo } from "@/components/SuoneloLogo";
import { ShellIcon } from "@/components/ui/shell-icon";
import {
  desktop, errorCode, errorMessage, type Account, type Bookmark, type BookmarkScope, type Credentials,
  type Destination, type LargePlan, type MetadataField, type QueueEvent, type QueueSnapshot, type SubmissionReport,
} from "@/lib/desktop";
import { isMac } from "@/lib/platform";
import { useSettings } from "@/lib/use-settings";

type ValidationOrigin = "settings" | "setup" | "notice";
type SignInMethod = "token" | "email";
type View = "Downloads" | "Bookmarks" | "Settings";
type Prompt =
  | { kind: "add" | "setup" }
  | { kind: "confirm"; title: string; description: string; label: string; action: () => Promise<void>; decline?: () => Promise<void>; restore?: Prompt | null; accountValidation?: boolean };

const previewNotice = "Qobuz offers MP3 previews for this account. Audio downloads will save previews where available.";

const emptyQueue: QueueSnapshot = {
  revision: -1, jobs: [], confirmations: [], canClear: false, protectedCount: 0, bytesPerSecond: 0,
  effectiveMode: "sequential", requestedMode: "sequential", liveLimit: 1, paused: false, canPause: false,
};

export default function App() {
  const [view, setView] = useState<View>("Downloads");
  const [filter, setFilter] = useState<Filter>("Show all");
  const [section, setSection] = useState<Section>("Account");
  const [queue, setQueue] = useState(emptyQueue);
  const [pauseBusy, setPauseBusy] = useState(false);
  const [bookmarks, setBookmarks] = useState<Bookmark[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const [account, setAccount] = useState<Account>();
  const [validation, setValidation] = useState<{ origin: ValidationOrigin; method: SignInMethod; processing: boolean } | null>(null);
  const [accountMessage, setAccountMessage] = useState("");
  const [accountErrorCode, setAccountErrorCode] = useState<string>();
  const [accountCommitRevision, setAccountCommitRevision] = useState(0);
  const startupSetupShown = useRef(false);
  const [metadata, setMetadata] = useState<MetadataField[]>([]);
  const [systemMode, setSystemMode] = useState<"light" | "dark">("light");
  const [prompt, setPrompt] = useState<Prompt | null>(null);
  const appRoot = useRef<HTMLDivElement>(null);
  const [searchOpen, setSearchOpen] = useState(false);
  const searchLauncher = useRef<HTMLButtonElement>(null);
  const [deciding, setDeciding] = useState<string[]>([]);
  const [toast, setToast] = useState<ToastMessage | null>(null);
  const [startupError, setStartupError] = useState("");
  const toastTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const bookmarkRevision = useRef(-1);
  const queueRevision = useRef(-1);
  const closeBusy = useRef(false);
  const lastAccountEvent = useRef<Account>(null);
  const manualValidation = useRef<string | null>(null);
  const cancellingValidation = useRef(false);

  const notify = useCallback((message: string, type: ToastType = "info") => {
    clearTimeout(toastTimer.current);
    setToast({ message, type });
    toastTimer.current = setTimeout(() => setToast(null), type === "error" ? 8000 : 4200);
  }, []);

  const { settings, theme: effectiveTheme, initialize, patch, flush, reset, resetVersion, saveStatus, saveLimit } = useSettings(notify);

  const acceptQueue = useCallback((incoming: QueueEvent) => {
    if (incoming.revision <= queueRevision.current) return;
    if ("jobs" in incoming) {
      queueRevision.current = incoming.revision;
      setQueue(incoming);
      return;
    }
    if (incoming.revision !== queueRevision.current + 1) {
      void desktop.queueSnapshot().then(acceptQueue).catch(() => undefined);
      return;
    }
    queueRevision.current = incoming.revision;
    if (!("jobId" in incoming)) {
      setQueue(previous => ({ ...previous, revision: incoming.revision, bytesPerSecond: incoming.bytesPerSecond }));
      return;
    }
    setQueue(previous => ({
      ...previous, revision: incoming.revision,
      jobs: previous.jobs.map(job => job.id === incoming.jobId && job.generation === incoming.generation &&
        job.files.some(file => file.id === incoming.fileId && incoming.attempt >= file.attempt) ? {
        ...job, progress: incoming.jobProgress,
        files: job.files.map(file => file.id === incoming.fileId
          ? { ...file, status: "downloading", generation: incoming.generation, attempt: incoming.attempt, progress: incoming.progress,
            phase: incoming.phase, receivedBytes: incoming.receivedBytes, totalBytes: incoming.totalBytes,
            finalSizeBytes: incoming.finalSizeBytes } : file),
      } : job),
    }));
  }, []);

  useEffect(() => {
    let active = true;
    let recovery: ReturnType<typeof setInterval> | undefined;
    const preference = matchMedia("(prefers-color-scheme: dark)");
    const syncSystem = () => setSystemMode(preference.matches ? "dark" : "light");
    const applyBookmarks = (incoming: Bookmark[]) => {
      setBookmarks(incoming);
      const ids = new Set(incoming.map(bookmark => bookmark.id));
      setSelected(previous => new Set([...previous].filter(id => ids.has(id))));
    };
    syncSystem();
    preference.addEventListener("change", syncSystem);
    const accountListener = listen<Account>("account-changed", ({ payload }) => {
      if (!active) return;
      lastAccountEvent.current = payload;
      setAccount(payload);
      if (!manualValidation.current) {
        setAccountMessage("");
        setAccountErrorCode(undefined);
      }
      const needsStartupSetup = payload.state === "missing" && payload.epoch === 0 && !startupSetupShown.current;
      if ((payload.state === "rejected" || needsStartupSetup) && !manualValidation.current) {
        if (needsStartupSetup) startupSetupShown.current = true;
        setPrompt(current => current ?? { kind: "setup" });
      }
    });
    void accountListener.catch(() => undefined).then(() => desktop.bootstrap()).then(initial => {
      if (!active) return;
      initialize(initial.settings, preference.matches);
      if (initial.settings.theme == null) void flush().catch(() => undefined);
      acceptQueue(initial.queue);
      applyBookmarks(initial.bookmarks);
      const account = lastAccountEvent.current ?? initial.account;
      setAccount(account);
      if ((account.state === "missing" || account.state === "rejected") && !manualValidation.current) {
        startupSetupShown.current = true;
        setPrompt(current => current ?? { kind: "setup" });
      }

      void desktop.metadataCatalog().then(catalog => {
        if (active) setMetadata(catalog);
      }).catch(error => { if (active) notify(errorMessage(error), "error"); });

      const recover = () => {
        if (!active || recovery) return;
        notify("Live updates disconnected. Refreshing the lists periodically.", "warning");
        recovery = setInterval(() => {
          void desktop.queueSnapshot().then(snapshot => { if (active) acceptQueue(snapshot); }).catch(() => undefined);
          void desktop.listBookmarks().then(items => { if (active) applyBookmarks(items); }).catch(() => undefined);
        }, 1500);
      };
      void desktop.subscribeQueue(snapshot => { if (active) acceptQueue(snapshot); }).then(() => preloadStage("queue")).catch(recover);
      void desktop.subscribeBookmarks(snapshot => {
        if (!active || snapshot.revision <= bookmarkRevision.current) return;
        bookmarkRevision.current = snapshot.revision;
        applyBookmarks(snapshot.bookmarks);
      }).then(() => preloadStage("bookmarks")).catch(recover);
    }).catch(error => {
      if (active) setStartupError(errorMessage(error));
    });

    return () => {
      active = false;
      void accountListener.then(unlisten => unlisten()).catch(() => undefined);
      preference.removeEventListener("change", syncSystem);
      clearInterval(recovery);
      clearTimeout(toastTimer.current);
    };
  }, [acceptQueue, flush, initialize, notify]);

  async function run(action: () => Promise<unknown>, success?: string) {
    try { await action(); if (success) notify(success, "success"); }
    catch (error) { notify(errorMessage(error), "error"); }
  }

  const dismiss = useCallback(() => setPrompt(null), []);

  useEffect(() => {
    if (view !== "Settings" || section !== "Artwork & Metadata") return;
    let current = true;
    void desktop.metadataCatalog().then(catalog => { if (current) setMetadata(catalog); }).catch(() => undefined);
    return () => { current = false; };
  }, [view, section]);

  const requestClose = useCallback(async () => {
    if (closeBusy.current) return;
    closeBusy.current = true;
    try {
      await flush();
      const decision = await desktop.requestClose();
      if (decision.requiresConfirmation) {
        setPrompt(previous => ({
          kind: "confirm",
          title: "Close this session?",
          description: "Active and queued work will stop. Session rows and staging files will be cleared. Finalized files will be kept.",
          label: "Close session",
          action: async () => { await flush(); await desktop.shutdownSession(); },
          restore: previous?.kind === "confirm" && previous.title === "Close this session?" ? previous.restore : previous,
        }));
      } else {
        await desktop.shutdownSession();
      }
    } catch (error) {
      notify(errorMessage(error), "error");
    } finally {
      closeBusy.current = false;
    }
  }, [flush, notify]);

  useEffect(() => {
    const registration = listen("close-requested", () => void requestClose());
    void registration.then(() => preloadStage("close")).catch(() => undefined);
    return () => { void registration.then(unlisten => unlisten()).catch(() => undefined); };
  }, [requestClose]);

  const plan = queue.confirmations.find(item => !deciding.includes(`${item.id}:${item.planRevision}`));
  const modalOpen = Boolean(prompt || plan || searchOpen);
  useEffect(() => {
    function shortcut(event: KeyboardEvent) {
      const primaryModifier = isMac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey;
      if (!settings || modalOpen || event.repeat || event.defaultPrevented || !primaryModifier || event.altKey || event.shiftKey) return;
      switch (event.key.toLowerCase()) {
        case "n":
          event.preventDefault();
          document.querySelector<HTMLButtonElement>('.pagehead [data-action="add"]')?.focus();
          setPrompt({ kind: "add" });
          break;
        case "f":
          event.preventDefault();
          searchLauncher.current?.focus();
          setSearchOpen(true);
          break;
      }
    }
    window.addEventListener("keydown", shortcut);
    return () => window.removeEventListener("keydown", shortcut);
  }, [modalOpen, settings]);

  const accountSettings = () => {
    setSection("Account");
    setView("Settings");
  };

  const cancelValidation = useCallback(async () => {
    const attempt = manualValidation.current;
    if (!attempt) return;
    manualValidation.current = null;
    cancellingValidation.current = true;
    setValidation(null);
    setAccountErrorCode(undefined);
    setAccountMessage("Sign-in cancelled.");
    try {
      await desktop.cancelAccountValidation(attempt);
    } catch (error) {
      setAccountMessage(errorMessage(error));
    } finally {
      cancellingValidation.current = false;
    }
  }, []);

  useEffect(() => {
    if ((validation?.origin === "settings" && (view !== "Settings" || section !== "Account")) ||
        (validation?.origin === "notice" && view === "Settings")) {
      void cancelValidation();
    }
  }, [view, section, validation?.origin, cancelValidation]);

  async function validate(input?: Credentials, origin: ValidationOrigin = "settings", method: SignInMethod = "token") {
    if (manualValidation.current || cancellingValidation.current || account?.state === "validating") return false;
    // WKWebView's packaged origin may not expose secure-context-only randomUUID().
    const attempt = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte =>
      byte.toString(16).padStart(2, "0"),
    ).join("");
    manualValidation.current = attempt;
    setValidation({ origin, method, processing: true });
    setAccountErrorCode(undefined);
    setAccountMessage(method === "email" ? "Opening Qobuz sign-in…" : "Validating credentials…");
    let awaitingConfirmation = false;

    function finish(account: Account) {
      setAccount(account);
      if (isMac) setAccountCommitRevision(revision => revision + 1);
      setAccountMessage(account.accessMode === "preview" ? "Signed in for MP3 previews." : "Signed in.");
      notify(account.accessMode === "preview" ? "Signed in to Qobuz for MP3 previews." : "Signed in to Qobuz.", "success");
    }

    function settle() {
      if (manualValidation.current === attempt) {
        manualValidation.current = null;
        setValidation(null);
      }
    }

    try {
      const result = method === "email"
        ? await desktop.startEmailSignIn(attempt, phase => {
          if (manualValidation.current !== attempt || awaitingConfirmation) return;
          setValidation({ origin, method, processing: phase === "validating" });
          setAccountMessage(phase === "waiting"
            ? "Complete sign-in in the Qobuz window. Select Back if Qobuz prompts you."
            : "Validating credentials…");
        })
        : await desktop.validateCredentials(attempt, input);
      if (manualValidation.current !== attempt) return false;
      if (result.account) {
        finish(result.account);
        if (origin === "setup") dismiss();
        return true;
      }
      if (!result.confirmation) throw Error("Qobuz credentials could not be validated.");
      const switching = result.confirmation === "switch";
      const restore = origin === "setup" ? { kind: "setup" } as const : null;
      awaitingConfirmation = true;
      setValidation({ origin, method, processing: false });
      setAccountMessage("Waiting for confirmation.");
      setPrompt({
        kind: "confirm",
        title: switching ? "Switch Qobuz accounts?" : "Stop active downloads?",
        description: switching
          ? "This will stop active downloads and clear the current Downloads queue. Saved files, bookmarks, and search ranking history will be kept."
          : "Active downloads will stop. Downloads rows and saved files will be kept.",
        label: switching ? "Switch accounts" : "Continue",
        restore,
        accountValidation: true,
        action: async () => {
          setValidation({ origin, method, processing: true });
          try {
            const committed = await desktop.confirmAccountValidation(attempt);
            if (manualValidation.current === attempt) {
              finish(committed);
              if (switching) {
                setExpanded(new Set());
                setDeciding([]);
                setFilter("Show all");
              }
            }
            dismiss();
          } catch (error) {
            setAccountMessage(errorMessage(error));
            setPrompt(restore);
          } finally {
            settle();
          }
        },
        decline: cancelValidation,
      });
      return false;
    } catch (error) {
      if (manualValidation.current === attempt) {
        setAccountMessage(errorMessage(error));
        if (method === "token") setAccountErrorCode(errorCode(error));
      }
      return false;
    } finally {
      if (!awaitingConfirmation) settle();
    }
  }

  function removeCredentials() {
    setPrompt({
      kind: "confirm",
      title: "Remove saved credentials?",
      description: "Active attempts will stop. Settings, bookmarks, and finalized files will be kept.",
      label: "Remove credentials",
      action: async () => {
        setAccount(await desktop.removeCredentials());
        setAccountMessage("");
        if (isMac) setAccountErrorCode(undefined);
        accountSettings();
      },
    });
  }

  function resetSettings() {
    setPrompt({
      kind: "confirm",
      title: "Reset settings?",
      description: "This restores default preferences and keeps credentials, bookmarks, and current jobs.",
      label: "Reset settings",
      action: async () => { await reset(); notify("Settings restored to defaults.", "success"); },
    });
  }

  function clearDownloads() {
    setPrompt({
      kind: "confirm",
      title: "Clear download list?",
      description: "Queued entries and inactive history will be removed. Active albums will continue. Files on disk will be kept.",
      label: "Clear all",
      action: async () => {
        const result = await desktop.clearDownloads();
        notify(`${result.removed} entries cleared. ${result.protected} active albums retained.`, "success");
      },
    });
  }

  function submission(report: SubmissionReport, destination: Destination) {
    if (!report.accepted.length && !report.invalid.length && !report.duplicates.length) return;
    if (report.accepted.length) {
      setView(destination === "downloads" ? "Downloads" : "Bookmarks");
      if (destination === "downloads") setFilter("Show all");
    }
    const summary = `${report.accepted.length} ${destination === "downloads" ? "queued" : "saved"} · ${report.duplicates.length} duplicate · ${report.invalid.length} invalid`;
    notify(summary, report.invalid.length || report.duplicates.length ? "warning" : report.accepted.length ? "success" : "info");
  }

  async function decide(item: LargePlan, decision: boolean) {
    await desktop.decideLargeSubmission(item, decision);
    const key = `${item.id}:${item.planRevision}`;
    setDeciding(previous => [...previous, key]);
    void desktop.queueSnapshot().then(acceptQueue).catch(() => undefined);
  }

  function bookmarkDownload(scope: BookmarkScope) {
    void run(async () => { await flush(); submission(await desktop.downloadBookmarks(scope), "downloads"); });
  }

  function toggle(setter: typeof setSelected, id: number) {
    setter(previous => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  const effectiveMode = settings?.appearance === "system" ? systemMode : (settings?.appearance ?? systemMode);
  const pending = queue.jobs.filter(job => job.status === "queued").map(job => job.id);
  const transfers = queue.jobs.reduce((count, job) => count + job.files.filter(file => file.status === "downloading").length, 0);
  const ready = Boolean(settings && account);
  const subtitle = view === "Downloads" ? (filter === "Show all" ? "Your current download session" : filter)
    : view === "Bookmarks" ? "Saved releases and links" : saveStatus;

  return (
    <div ref={appRoot} id="app" data-mode={effectiveMode} data-theme={effectiveTheme} data-loading={!ready || undefined}>
      <header className="top">
        <div className="brand"><SuoneloLogo /></div>
        {ready && <button ref={searchLauncher} type="button" className="search-launcher" aria-haspopup="dialog" aria-keyshortcuts={isMac ? "Meta+F" : "Control+F"}
          disabled={Boolean(prompt || plan)} onClick={() => setSearchOpen(true)}>
          <Search className="icon" aria-hidden /><span>Search Qobuz</span><kbd>{isMac ? "⌘F" : "Ctrl+F"}</kbd>
        </button>}
        <div className="topright">
          <Button className="ghost with-icon" onClick={() => void requestClose()}><ShellIcon name="exit" /> Exit</Button>
        </div>
      </header>
      <aside aria-label="Application navigation">
        <div className="section-label">Library</div>
        <Button aria-current={view === "Downloads"} onClick={() => setView("Downloads")}>
          <ShellIcon name="downloads" /> Downloads <span className="count">{queue.jobs.length}</span>
        </Button>
        <Button aria-current={view === "Bookmarks"} onClick={() => setView("Bookmarks")}>
          <ShellIcon name="bookmarks" /> Bookmarks <span className="count">{bookmarks.length}</span>
        </Button>
        {view === "Downloads" && (
          <>
            <div className="section-label">Queue status</div>
            {filters.map(name => (
              <Button key={name} aria-current={filter === name} onClick={() => setFilter(name)}>
                {name} <span className="count">{queue.jobs.filter(job => matches(job, name)).length}</span>
              </Button>
            ))}
          </>
        )}
        <div className="sidebar-footer">
          <div className="section-label">Application</div>
          <Button aria-current={view === "Settings"} onClick={() => setView("Settings")}><ShellIcon name="settings" /> Settings</Button>
        </div>
      </aside>
      <main>
        <header className="pagehead">
          <div><h1>{view}</h1><p className="subtitle" role={view === "Settings" ? "status" : undefined}>{subtitle}</p></div>
          {ready && <div className="actions">
            {view === "Settings" ? <Button className="danger" data-action="reset-settings" onClick={resetSettings}>Reset settings</Button>
              : <>
                {view === "Downloads" && queue.jobs.length > 0 && (
                  <Button className="danger with-icon" disabled={!queue.canClear} onClick={clearDownloads}>Clear all</Button>
                )}
                {view === "Downloads" && queue.canPause && <Button className="with-icon" disabled={pauseBusy} onClick={() => {
                  setPauseBusy(true);
                  void run(async () => acceptQueue(await desktop.setQueuePaused(!queue.paused)))
                    .finally(() => setPauseBusy(false));
                }}>
                  {queue.paused ? <Play className="icon" aria-hidden /> : <CirclePause className="icon" aria-hidden />}
                  {queue.paused ? "Resume" : "Pause"}
                </Button>}
                <AddLinkButton onClick={() => setPrompt({ kind: "add" })} />
              </>}
          </div>}
        </header>
        {startupError && (
          <section className="panel config" role="alert">
            <h2>Desktop connection unavailable</h2>
            <p>{startupError}</p>
            <Button onClick={() => window.location.reload()}>Retry</Button>
          </section>
        )}
        {!ready && !startupError && <div className="panel empty" role="status">Loading the desktop session…</div>}
        {ready && account && settings && (
          <>
            {view !== "Settings" && account.state !== "ready" && (
              <div className="session-notice notice">
                <span>{accountMessage || account.reason || `${accountLabels[account.state]}. Authenticated actions will wait.`}</span>
                {account.state === "unvalidated" ? (
                  <ValidationButton label="Retry validation" validating={validation?.origin === "notice" && validation.processing}
                    disabled={!!validation} onClick={() => void validate(undefined, "notice")} />
                ) : account.state !== "validating" && (
                  <Button onClick={() => setPrompt({ kind: "setup" })}>
                    {account.state === "rejected" ? "Replace token" : "Set up account"}
                  </Button>
                )}
              </div>
            )}
            {view === "Downloads" && account.state === "ready" && account.accessMode === "preview" && (
              <div className="session-notice notice" role="status">
                {isMac ? <span>{previewNotice}</span> : previewNotice}
              </div>
            )}
            {view !== "Settings" && settings.outputMissing && (
              <div className="session-notice notice">
                <span>The download destination is unavailable.</span>
                <Button onClick={() => { setSection("Output"); setView("Settings"); }}>Choose a destination</Button>
              </div>
            )}
            {view === "Downloads" && (
              <>
                {queue.recovery?.retryConnection ? (
                  <div className="session-notice notice">
                    <span role="status">Qobuz requests are waiting for a connection check.</span>
                    <Button onClick={() => void run(() => desktop.retryConnection())}>Retry connection</Button>
                  </div>
                ) : queue.recovery?.cooldownSeconds ? (
                  <p className="session-notice notice" role="status">
                    Qobuz rate limit. New API requests resume in {queue.recovery.cooldownSeconds} s.
                  </p>
                ) : queue.recovery?.workerCap ? (
                  <p className="session-notice notice" role="status">
                    Connection recovery limits the app to {queue.recovery.workerCap} active {queue.recovery.workerCap === 1 ? "operation" : "operations"}.
                  </p>
                ) : null}
                <Downloads jobs={queue.jobs} paused={queue.paused} filter={filter} expanded={expanded} toggle={id => toggle(setExpanded, id)} tooltipContainer={appRoot} actions={{
                  cancel: id => run(() => desktop.cancelJob(id)),
                  remove: id => run(() => desktop.removeJob(id)),
                  retry: id => run(async () => { await flush(); await desktop.retryJob(id); }),
                  retryFile: (id, fileId) => void run(async () => { await flush(); await desktop.retryFile(id, fileId); }),
                  replaceFile: (id, file) => {
                    const token = file.replacementToken;
                    if (!token || !file.conflictPath) return;
                    setPrompt({
                      kind: "confirm",
                      title: "Replace saved audio?",
                      description: `Replace ${file.title} at ${file.conflictPath} with this download?`,
                      label: "Replace audio",
                      action: async () => { await flush(); await desktop.replaceSearchAudio(id, file.id, token); },
                    });
                  },
                  openFolder: (id, fileId) => void run(() => desktop.openContainingFolder(id, fileId)),
                  openReleaseFolder: id => run(() => desktop.openReleaseFolder(id)),
                }} />
              </>
            )}
            {view === "Bookmarks" && (
              <Bookmarks
                bookmarks={bookmarks} selected={selected} toggle={id => toggle(setSelected, id)} deselect={() => setSelected(new Set())}
                download={bookmarkDownload}
                remove={ids => void run(() => desktop.removeBookmarks(ids), "Selected bookmarks removed.")}
                retry={id => void run(() => desktop.retryBookmark(id))}
                clear={() => setPrompt({
                  kind: "confirm", title: "Clear bookmarks?", description: "This removes every saved bookmark.",
                  label: "Clear all", action: () => desktop.clearBookmarks(),
                })}
              />
            )}
            {view === "Settings" && (
              <>
                <SettingsPage key={resetVersion} section={section} select={setSection} settings={settings} patch={patch} theme={effectiveTheme}
                  modePending={queue.requestedMode !== queue.effectiveMode}
                  retryTheme={settings.theme == null && saveStatus === "Settings could not be saved." ? () => void flush().catch(() => undefined) : undefined}
                  metadata={metadata} account={account} notify={notify}
                  pending={Boolean(pending.length)} apply={() => void run(async () => {
                    await flush();
                    await desktop.applySettingsToPending(pending);
                  }, "Applied current settings to queued jobs.")}
                  accountActions={{
                    validate, signIn: () => void validate(undefined, "settings", "email"),
                    remove: removeCredentials, cancel: () => void cancelValidation(), clearMessage: () => { setAccountMessage(""); setAccountErrorCode(undefined); },
                    busy: !!validation, validating: validation?.origin === "settings" && validation.processing, message: accountMessage, errorCode: accountErrorCode ?? account.reasonCode, commitRevision: accountCommitRevision,
                  }} />
              </>
            )}
          </>
        )}
      </main>
      <footer className="foot">
        <span>{account?.state === "ready" && account.accessMode === "preview" ? "MP3 previews only" : account ? accountLabels[account.state] : "Starting"} · {transfers} active {transfers === 1 ? "transfer" : "transfers"}</span>
        <SpeedControl queue={queue} saved={settings?.maxConnections ?? 1} disabled={!ready || modalOpen}
          save={async value => {
            try {
              await saveLimit(value, acceptQueue);
              notify("Max connections saved.", "success");
            } catch (error) {
              notify(errorMessage(error), "error");
              throw error;
            }
          }} />
      </footer>
      <Toast toast={toast} dismiss={() => { clearTimeout(toastTimer.current); setToast(null); }} />
      {ready && <CatalogSearch key={account?.epoch ?? 0} open={searchOpen && !prompt} close={() => setSearchOpen(false)}
        account={account} beforeQueue={flush} onSubmit={(report, destination) => {
          const target = destination === "downloads" ? "Downloads" : "Bookmarks";
          const skipped = report.skipped.length + report.conflicts.length;
          const message = `${report.accepted.length} added to ${target}${skipped ? ` · ${skipped} skipped` : ""}.`;
          notify(message, report.accepted.length ? "success" : "info");
        }} />}
      {prompt?.kind === "add" && settings && <AddLinks close={dismiss} onSuccess={submission} beforeQueue={flush}
        initialQuality={settings.quality} allowFallback={settings.allowFallback} previewOnly={account?.accessMode === "preview"} tokenForm={isMac && account?.accessMode === "unknown"} mode={`${effectiveMode ?? systemMode}:${effectiveTheme}`} />}
      {(prompt?.kind === "setup" || (isMac && prompt?.kind === "confirm" && prompt.restore?.kind === "setup")) && <AccountSetupDialog finish={input => void validate(input, "setup")}
        signIn={() => void validate(undefined, "setup", "email")} cancel={() => void cancelValidation()}
        close={() => { void cancelValidation(); dismiss(); }}
        busy={validation?.origin === "setup"} validating={validation?.origin === "setup" && validation.processing}
        signingIn={validation?.origin === "setup" && validation.method === "email"}
        reason={account?.reason} message={accountMessage} errorCode={accountErrorCode ?? account?.reasonCode} hasSavedToken={account?.hasToken} />}
      {prompt?.kind === "confirm" && (
        <Confirmation key={prompt.title} title={prompt.title} description={prompt.description} label={prompt.label}
          validateAccount={prompt.accountValidation}
          confirm={async () => { await prompt.action(); if (!prompt.accountValidation) dismiss(); }}
          close={async () => { await prompt.decline?.(); setPrompt(prompt.restore ?? null); }} />
      )}
      {!prompt && !searchOpen && plan && (
        <Confirmation key={`${plan.id}:${plan.planRevision}`} title={`Download ${plan.trackCount} ${plan.unit === "pdfs" ? "booklets" : "tracks"}?`}
          description="Transfers will wait for your confirmation."
          label="Start downloads" confirm={() => decide(plan, true)} close={() => decide(plan, false)} />
      )}
    </div>
  );
}
