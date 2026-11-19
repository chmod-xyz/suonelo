import { automaticSetupRetry, isMac } from "@/lib/platform";
import { readText } from "@tauri-apps/plugin-clipboard-manager";
import { Bookmark, Check, Clipboard, Eye, EyeOff, FileText, Plus, Upload, X } from "lucide-react";
import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode, type RefObject } from "react";
import {
  automaticSetupFailure, desktop, errorMessage, type Credentials, type Destination, type SubmissionReport,
} from "@/lib/desktop";
import { Button, Field, ValidationButton } from "./ui/primitives";
import { QualityPicker } from "./quality-picker";
import { useLinkImport } from "@/lib/use-link-import";

export function Modal({ title, description, children, cancel, busy, dialogRef, className, motion, heading }: {
  title: string; description?: string; children: ReactNode; cancel: () => void; busy?: boolean;
  dialogRef?: RefObject<HTMLDialogElement | null>;
  className?: string;
  motion?: "enter" | "exit";
  heading?: (titleId: string) => ReactNode;
}) {
  const id = useId();
  const ownRef = useRef<HTMLDialogElement>(null);
  const dialog = dialogRef ?? ownRef;
  useEffect(() => {
    const element = dialog.current;
    if (!element) return;
    const trigger = document.activeElement;
    element.showModal();
    element.querySelector<HTMLElement>("[data-initial-focus]")?.focus();
    return () => {
      element.close();
      if (trigger instanceof HTMLElement && trigger.isConnected) trigger.focus({ preventScroll: true });
    };
  }, []);

  return (
    <dialog ref={dialog} className={className} data-motion={motion} inert={motion === "exit"}
      aria-labelledby={`${id}-title`} aria-describedby={description ? `${id}-description` : undefined}
      onCancel={event => { event.preventDefault(); if (!busy) cancel(); }}>
      {heading ? heading(`${id}-title`) : <h2 id={`${id}-title`}>{title}</h2>}
      {description && <p id={`${id}-description`}>{description}</p>}
      {children}
    </dialog>
  );
}

export function AddLinks({ close, onSuccess, beforeQueue, initialQuality, allowFallback, previewOnly, tokenForm, mode }: {
  close: () => void; onSuccess: (report: SubmissionReport, destination: Destination) => void;
  beforeQueue: () => Promise<void>;
  initialQuality: number; allowFallback: boolean; previewOnly?: boolean; tokenForm?: boolean; mode: string;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [report, setReport] = useState<SubmissionReport>();
  const [leaving, setLeaving] = useState(false);
  const exit = useRef<ReturnType<typeof setTimeout>>(undefined);
  const input = useRef<HTMLTextAreaElement>(null);
  const dialog = useRef<HTMLDialogElement>(null);
  const file = useLinkImport(dialog, input, busy || leaving);
  const [quality, setQuality] = useState(initialQuality);
  const attachment = file.attachment;
  const sourceText = attachment?.content ?? text;
  const inputMode = file.reading ? "checking" : attachment ? "attached" : file.hover ? "drag" : "text";
  const hintId = useId();
  const importErrorId = useId();

  useEffect(() => () => clearTimeout(exit.current), []);

  function dismiss() {
    if (exit.current !== undefined) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      close();
      return;
    }
    const element = dialog.current;
    if (element) {
      const frame = getComputedStyle(element);
      element.style.setProperty("--add-exit-transform", frame.transform);
      element.style.setProperty("--add-exit-opacity", frame.opacity);
    }
    setLeaving(true);
    exit.current = setTimeout(close, 210);
  }

  useEffect(() => {
    if (!attachment) return;
    const frame = requestAnimationFrame(() => dialog.current?.querySelector<HTMLElement>(".attachment-remove")?.focus({ preventScroll: true }));
    return () => cancelAnimationFrame(frame);
  }, [attachment]);

  async function submit(destination: Destination) {
    if (busy || file.reading || !sourceText.trim()) return;
    setBusy(true);
    setError("");
    setReport(undefined);
    try {
      if (destination === "downloads") await beforeQueue();
      const submit = attachment ? desktop.submitImportedLinks : desktop.submitLinks;
      const result = await submit(sourceText, destination, destination === "downloads" && !previewOnly ? quality : undefined);
      onSuccess(result, destination);
      if (result.invalid.length || result.duplicates.length) {
        setReport(result);
      } else if (result.accepted.length) {
        dismiss();
      }
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setBusy(false);
    }
  }

  async function paste() {
    const request = file.generation.current;
    try {
      const pasted = await readText();
      if (!file.alive.current || request !== file.generation.current) return;
      setText(pasted);
      setError("");
      setReport(undefined);
      file.clearError();
      input.current?.focus();
    } catch {
      setError(`Clipboard text is unavailable. Press ${isMac ? "⌘V" : "Ctrl+V"} in the URL field to paste.`);
    }
  }

  return (
    <Modal title="Add links" description="Paste one or more Qobuz URLs. Put each link on a new line."
      cancel={dismiss} busy={busy || leaving} dialogRef={dialog} className="add-links" motion={leaving ? "exit" : "enter"}>
      <form onSubmit={event => { event.preventDefault(); void submit("downloads"); }}>
        <div className="actions import-toolbar">
          <Button className="with-icon" disabled={busy || file.reading || Boolean(attachment)} onClick={paste}>
            <Clipboard className="icon" aria-hidden /> Paste from clipboard
          </Button>
          <Button className="with-icon" disabled={busy || file.reading || Boolean(attachment)} onClick={file.choose}>
            <FileText className="icon" aria-hidden /> Add TXT file
          </Button>
        </div>
        <div className="import-region" data-mode={inputMode} aria-busy={file.reading}>
          <div className="import-panel import-text" inert={inputMode !== "text"} aria-hidden={inputMode !== "text"}>
            <Field label="Qobuz URLs">
              <textarea ref={input} data-initial-focus spellCheck={false} value={text}
                aria-describedby={`${hintId} ${importErrorId}`} placeholder="https://play.qobuz.com/album/…"
                onChange={event => { setText(event.target.value); setError(""); setReport(undefined); file.clearError(); }} />
            </Field>
          </div>
          <div className="import-panel import-drop" inert={inputMode !== "drag"} aria-hidden={inputMode !== "drag"}>
            <Upload className="icon" aria-hidden /><strong>Drop one text file here</strong><p>UTF-8 .txt · up to 2 MB</p>
          </div>
          <div className="import-panel import-check" inert={inputMode !== "checking"} aria-hidden={inputMode !== "checking"}>
            <span className="import-spinner" aria-hidden /> Checking text file…
            <Button className="ghost" onClick={file.remove}>Cancel import</Button>
          </div>
          <div className="import-panel import-attached" inert={inputMode !== "attached"} aria-hidden={inputMode !== "attached"}>
            {attachment && (
              <div className="attachment-card">
                <span className="attachment-file"><FileText className="icon" aria-hidden /></span>
                <div className="attachment-copy">
                  <strong className="attachment-name">{attachment.filename}</strong>
                  <span className="attachment-meta">TXT · {attachment.sizeBytes.toLocaleString("en-US")} bytes · UTF-8</span>
                  <span className="attachment-valid"><Check className="icon" aria-hidden /> Valid · {attachment.totalUrls} URLs · {attachment.uniqueUrls} unique
                    {attachment.duplicates > 0 && ` · ${attachment.duplicates} duplicates`}</span>
                </div>
                <Button className="attachment-remove" disabled={busy} aria-label={`Remove ${attachment.filename}`}
                  onClick={() => { file.remove(); setReport(undefined); setError(""); }}>
                  <X className="icon" aria-hidden />
                </Button>
              </div>
            )}
          </div>
        </div>
        <p className="import-hint" id={hintId}>Drag and drop one .txt file into this dialog, or choose Add TXT file. UTF-8 · up to 2 MB.</p>
        <div className="import-announcement" role="status">{file.announcement}</div>
        <div className="import-error" id={importErrorId} role="alert">
          {file.error && <p>{file.error}</p>}
          {file.issues.length > 0 && <ul>{file.issues.map((issue, index) => <li key={index}>Line {issue.line} · {issue.url} · {issue.reason}</li>)}</ul>}
        </div>
        {previewOnly ? <p className="notice">Audio downloads will save MP3 previews where available.</p>
          : <QualityPicker value={quality} change={setQuality} allowFallback={allowFallback} previewFallback={tokenForm} mode={mode} disabled={busy} />}
        {error && <p className="notice error" role="alert">{error}</p>}
        {report && (
          <div className="submission-report" role="alert">
            <div>{report.accepted.length} accepted · {report.duplicates.length} duplicate · {report.invalid.length} invalid</div>
            <ul>
              {report.accepted.map((url, index) => <li className="accepted" key={`accepted-${index}`}>{url} — accepted.</li>)}
              {report.duplicates.map((url, index) => <li key={`duplicate-${index}`}>{url} — already in this destination.</li>)}
              {report.invalid.map(({ url, reason }, index) => <li key={`invalid-${index}`}>{url} — {reason}</li>)}
            </ul>
          </div>
        )}
        <div className="actions">
          <Button className="with-icon" disabled={busy} onClick={dismiss}><X className="icon" aria-hidden /> {report?.accepted.length ? "Close" : "Cancel"}</Button>
          <Button className="with-icon" disabled={busy || file.reading || !sourceText.trim()} onClick={() => submit("bookmarks")}>
            <Bookmark className="icon" aria-hidden /> Bookmark
          </Button>
          <Button className="primary with-icon" disabled={busy || file.reading || !sourceText.trim()} type="submit"><Plus className="icon" aria-hidden /> Add to queue</Button>
        </div>
      </form>
    </Modal>
  );
}

export function Confirmation({ title, description, label, confirm, close, cancelLabel = "Cancel", validateAccount = false }: {
  title: string; description: string; label: string; confirm: () => Promise<void>; close: () => Promise<void> | void;
  cancelLabel?: string;
  validateAccount?: boolean;
}) {
  const [busy, setBusy] = useState<"confirm" | "cancel" | null>(null);
  const [error, setError] = useState("");
  async function act(action: () => Promise<void> | void, phase: "confirm" | "cancel") {
    if (busy) return;
    setBusy(phase);
    setError("");
    try { await action(); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(null); }
  }
  return (
    <Modal title={title} description={description} cancel={() => void act(close, "cancel")} busy={!!busy}>
      {error && <p className="notice error" role="alert">{error}</p>}
      <div className="actions">
        <Button data-initial-focus disabled={!!busy} onClick={() => act(close, "cancel")}>{cancelLabel}</Button>
        {validateAccount
          ? <ValidationButton className="primary" label={label} validating={busy === "confirm"} disabled={!!busy} onClick={() => act(confirm, "confirm")} />
          : <Button className="primary" disabled={!!busy} onClick={() => act(confirm, "confirm")}>{label}</Button>}
      </div>
    </Modal>
  );
}

export function SecretField({ label, value, change, disabled, initialFocus, placeholder }: {
  label: string; value: string; change: (value: string) => void; disabled: boolean; initialFocus?: boolean; placeholder?: string;
}) {
  const id = useId();
  const [visible, setVisible] = useState(false);
  const Icon = visible ? EyeOff : Eye;
  return (
    <div className="field setup-password">
      <label htmlFor={id}>{label}</label>
      <input id={id} data-initial-focus={initialFocus} type={visible ? "text" : "password"}
        autoComplete="off" spellCheck={false} disabled={disabled} value={value} placeholder={placeholder}
        onChange={event => change(event.target.value)} />
      <Button type="button" className="ghost setup-password-eye" disabled={disabled}
        aria-label={`${visible ? "Hide" : "Show"} ${label === "App secret" ? "app secret" : label}`}
        onClick={() => setVisible(current => !current)}>
        <Icon className="icon" aria-hidden />
      </Button>
    </div>
  );
}

export function AccountSetupDialog({ close, finish, signIn, cancel, reason, busy, validating, signingIn, message, errorCode, hasSavedToken }: {
  close: () => void;
  finish: (input: Credentials) => void;
  signIn: () => void;
  cancel: () => void;
  reason?: string | null;
  busy: boolean;
  validating: boolean;
  signingIn: boolean;
  message: string;
  errorCode?: string | null;
  hasSavedToken?: boolean;
}) {
  const [token, setToken] = useState("");
  const [appId, setAppId] = useState("");
  const [secret, setSecret] = useState("");
  const [manual, setManual] = useState(false);
  const enteredId = manual ? appId.trim() : "";
  const enteredSecret = manual ? secret.trim() : "";
  const canRetry = automaticSetupFailure(errorCode) && (Boolean(token.trim()) || hasSavedToken);
  const appPairComplete = Boolean(enteredId) === Boolean(enteredSecret);
  const canSubmit = Boolean(token.trim()) && appPairComplete;

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!canSubmit || busy) return;
    finish({ token: token.trim(), appId: enteredId, secret: enteredSecret });
  }

  return (
    <Modal title="Set up your Qobuz account"
      description={`Use a Qobuz token or sign in with your email and password. ${isMac ? "macOS Keychain" : "Windows"} protects your saved session.`}
      cancel={close}>
      <form onSubmit={submit}>
        <SecretField label="Qobuz token" value={token} change={setToken} disabled={busy} initialFocus />
        <Button className="ghost" aria-expanded={manual} disabled={busy} onClick={() => setManual(open => !open)}>
          Enter app credentials manually
        </Button>
        {manual && <div className="app-credentials">
          <Field label="App ID"><input autoComplete="off" disabled={busy} value={appId} onChange={event => setAppId(event.target.value)} /></Field>
          <SecretField label="App secret" value={secret} change={setSecret} disabled={busy} />
          {isMac && <p className="dim">suonelo saves a validated manual pair for future token sign-ins on this profile.</p>}
        </div>}
        {!appPairComplete && <p className="notice error" role={isMac ? "status" : undefined}>Enter both app fields or leave both empty.</p>}
        {(message || reason) && <p className="notice" role="status">{message || reason}</p>}
        <div className="actions">
          <Button onClick={busy ? cancel : close}>{busy ? (signingIn ? "Cancel sign-in" : "Cancel validation") : "Set up later"}</Button>
          {canRetry && <Button disabled={busy} onClick={() => finish(automaticSetupRetry(token.trim()))}>Retry automatic setup</Button>}
          <ValidationButton type="submit" className="primary" label="Validate" validating={validating && !signingIn} disabled={busy || !canSubmit} />
        </div>
        <div className="account-alternative">
          <ValidationButton className="ghost" label="Sign in with email and password" validating={validating && signingIn}
            disabled={busy} onClick={signIn} />
        </div>
      </form>
    </Modal>
  );
}
