import { automaticSetupRetry, isMac } from "@/lib/platform";
import { Monitor, Moon, Sun } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";
import {
  automaticSetupFailure, type Account, type Credentials, type MetadataField, type Mode,
  type Settings, type SettingsPatch, type Theme,
} from "@/lib/desktop";
import { OutputSettings } from "./output-settings";
import type { Notify } from "./toast";
import { Button, Field, SettingSwitch, ValidationButton } from "./ui/primitives";
import { AnimatedSelect } from "./ui/animated-select";
import { MovingChoices } from "./ui/moving-choices";
import { ConnectionSlider } from "./connection-controls";
import { SecretField } from "./dialogs";
import { About } from "./about";
import { PrivacyPolicy } from "./privacy-policy";
import { AdvancedMetadata } from "./advanced-metadata";
import { animateAppearance } from "@/lib/appearance";

export const sections = ["Account", "Download", "Output", "Artwork & Metadata", "Appearance", "About", "Privacy"] as const;
export type Section = typeof sections[number];
type Edit = (changes: SettingsPatch) => void;

const descriptions: Partial<Record<Section, string>> = {
  Download: "Choose the audio format and how quickly links start.",
  Output: "Click a badge to insert it into the focused field, or drag it into either template.",
  Account: "Switching Qobuz accounts clears Downloads after confirmation. Saved files, bookmarks, and profile ranking history are kept.",
  Appearance: "Follow the system appearance or choose a mode and theme.",
};

const qualityOptions = [
  "1 · MP3 · 320 kbps", "2 · FLAC · 16-bit / 44.1 kHz",
  "3 · FLAC · up to 24-bit / 96 kHz", "4 · FLAC · up to 24-bit / 192 kHz",
].map((label, index) => ({ value: index + 1, label }));
const lyricsOptions = [
  { value: "off", label: "Do not save" }, { value: "embed", label: "Embed in file" },
  { value: "external", label: "Save as external file" },
] as const;
const artworkOptions = [
  { value: "original", label: "Original" }, { value: "1200", label: "1200 px" },
  { value: "600", label: "600 px" }, { value: "300", label: "300 px" },
] as const;

const themes: Record<Theme, string> = {
  Suonelo: "suonelo",
  Vercel: "I don't like colors", Spotify: "Avocado",
  "Material Design": "Prune", Claymorphism: "Papaya", Violet: "Ube",
};

const modes = [
  { id: "system", label: "Match system", Icon: Monitor },
  { id: "light", label: "Light", Icon: Sun },
  { id: "dark", label: "Dark", Icon: Moon },
] satisfies { id: Mode; label: string; Icon: typeof Monitor }[];

function DownloadSettings({ settings, patch, pending, apply, modePending }: {
  settings: Settings; patch: Edit; pending: boolean; apply: () => void; modePending: boolean;
}) {
  const [interval, setIntervalValue] = useState(String(settings.intervalSeconds));
  const sequential = settings.downloadMode === "sequential";
  return (
    <>
      <AnimatedSelect label="Requested quality" value={settings.quality} items={qualityOptions} change={quality => patch({ quality })} />
      <Field label="Interval between link starts (seconds)">
        <input type="number" min={0} step={1} value={interval} onChange={event => {
          const value = event.currentTarget.valueAsNumber;
          setIntervalValue(event.currentTarget.value);
          if (Number.isSafeInteger(value) && value >= 0) patch({ intervalSeconds: value });
        }} />
      </Field>
      <p className="field-title" id="download-mode">Download mode</p>
      <MovingChoices className="mode-picker download-mode" role="group" aria-labelledby="download-mode" value={settings.downloadMode}>
        <Button data-choice="sequential" aria-pressed={sequential} onClick={() => patch({ downloadMode: "sequential" })}>Sequential</Button>
        <Button data-choice="concurrent" aria-pressed={!sequential} onClick={() => patch({ downloadMode: "concurrent" })}>Concurrent</Button>
      </MovingChoices>
      {modePending && <p className="notice" role="status">Download mode change pending until active jobs finish.</p>}
      <ConnectionSlider label={sequential ? "Max connections" : "Max connections per job"} value={settings.maxConnections}
        change={maxConnections => patch({ maxConnections })} />
      <p>{sequential
        ? "One job at a time. Auto allows four transfers and Unlimited allows up to 32, subject to recovery safeguards."
        : "Multiple jobs share eight transfers. Auto allows four per job and Unlimited uses the shared ceiling."}</p>
      <AnimatedSelect label="Lyrics" value={settings.lyricsMode} items={lyricsOptions} change={lyricsMode => patch({ lyricsMode })} />
      <p>Qobuz lyrics are unavailable. Enabled lyrics modes use LRCLIB.</p>
      <SettingSwitch name="albumsOnly" label="Limited to album releases" checked={settings.limitedToAlbumReleases}
        description="Skip singles, EPs, and Various Artists compilations when processing releases."
        change={limitedToAlbumReleases => patch({ limitedToAlbumReleases })} />
      <SettingSwitch name="allowFallback" label="Allow fallback" checked={settings.allowFallback}
        description="Use a lower FLAC quality down to CD quality. When off, skip tracks unavailable at the requested quality."
        change={allowFallback => patch({ allowFallback })} />
      <SettingSwitch name="bookletOnly" label="Booklet only" checked={settings.bookletOnly}
        description="Save available PDF booklets without downloading audio, artwork, or lyrics."
        change={bookletOnly => patch({ bookletOnly })} />
      <Button data-action="apply-queued" disabled={!pending} onClick={apply}>Apply to queued jobs</Button>
    </>
  );
}

function invalidCoverFilename(text: string) {
  const value = text.trim();
  return new TextEncoder().encode(value).length > 220 || /[<>:"/\\|?*\u0000-\u001f\u007f-\u009f]/.test(value) || value.endsWith(".")
    || /^(con|prn|aux|nul|conin\$|conout\$|com[1-9¹²³]|lpt[1-9¹²³]) *(\.|$)/i.test(value)
    || /\.(avif|bmp|gif|heic|heif|ico|jpe?g|png|svg|tiff?|webp)$/i.test(value);
}

function ArtworkMetadata({ settings, patch, metadata }: { settings: Settings; patch: Edit; metadata: MetadataField[] }) {
  const [filename, setFilename] = useState(settings.coverFilename);
  const invalid = invalidCoverFilename(filename);
  const eligible = metadata.filter(field => field.eligible !== false);
  const raw = eligible.filter(field => field.rawPath || /^(album|track)\//.test(field.id));
  const standard = eligible.filter(field => !raw.includes(field));
  return (
    <>
      <section className="settings-group" aria-labelledby="artwork-heading">
        <h3 id="artwork-heading">Artwork</h3>
        <p>Save the best available cover up to the chosen size beside the audio.</p>
        <SettingSwitch name="downloadCover" label="Download cover art" checked={settings.downloadCoverArt}
          description="Save a separate image beside the audio and enable artwork options."
          change={downloadCoverArt => {
            if (!downloadCoverArt && invalid) setFilename(settings.coverFilename);
            patch({ downloadCoverArt });
          }} />
        <fieldset className="artwork-options" aria-label="Artwork options" disabled={!settings.downloadCoverArt}>
          <SettingSwitch name="embedArt" label="Embed cover art" checked={settings.embedCoverArt}
            description="Put the cover image inside each downloaded audio file."
            change={embedCoverArt => patch({ embedCoverArt })} />
          <AnimatedSelect label="Artwork resolution" value={settings.artwork} items={artworkOptions}
            disabled={!settings.downloadCoverArt} change={artwork => patch({ artwork })} />
          <Field label="Cover art filename">
            <input value={filename} placeholder="cover" aria-invalid={invalid}
              aria-describedby="cover-name-help cover-name-error" onChange={event => {
                const value = event.target.value;
                setFilename(value);
                if (!invalidCoverFilename(value)) patch({ coverFilename: value.trim() });
              }} />
            <small id="cover-name-help">Enter a name without an extension. Leave it blank to use cover.</small>
            <span id="cover-name-error" className="field-error" aria-live="polite">
              {invalid ? "Use a valid filename without an image extension." : ""}
            </span>
          </Field>
        </fieldset>
      </section>
      <section className="settings-group" aria-labelledby="metadata-heading">
        <h3 id="metadata-heading">Metadata</h3>
        <p>Disabled fields leave source tags intact. Additional fields are opt-in.</p>
        <div className="meta-grid">
          {standard.map(field => (
            <label key={field.id} className="check" title={field.description}>
              <input type="checkbox" checked={settings.tags[field.id] ?? field.defaultEnabled}
                onChange={event => patch({ tags: { ...settings.tags, [field.id]: event.target.checked } })} />
              {field.label}
            </label>
          ))}
          {!metadata.length && <span className="dim">The metadata catalog is unavailable.</span>}
        </div>
        <AdvancedMetadata fields={raw} settings={settings} patch={patch} />
      </section>
    </>
  );
}

export const accountLabels: Record<Account["state"], string> = {
  missing: "Account setup needed",
  validating: "Validating credentials",
  unvalidated: "Credentials need validation",
  ready: "Signed in",
  rejected: "Qobuz rejected the saved token",
};

function AccountSettings({ account, validate, signIn, remove, cancel, clearMessage, busy, validating, message, errorCode, commitRevision }: AccountActions & { account: Account }) {
  const [method, setMethod] = useState<"token" | "email">("token");
  const [token, setToken] = useState("");
  const [appId, setAppId] = useState("");
  const [secret, setSecret] = useState("");
  const [maskRevision, resetMask] = useState(0);
  const [manual, setManual] = useState(false);
  const [tokenFeedback, setTokenFeedback] = useState<{ message: string; code: string }>();
  const disabled = busy || account.state === "validating";
  const byEmail = method === "email";
  const enteredId = manual ? appId.trim() : "";
  const enteredSecret = manual ? secret.trim() : "";
  const entered = Boolean(token.trim() || enteredId || enteredSecret);
  const restoredFeedback = isMac && !byEmail && !message ? tokenFeedback : undefined;
  const feedback = message || restoredFeedback?.message || account.reason;
  const canRetry = automaticSetupFailure(errorCode ?? restoredFeedback?.code) && (Boolean(token.trim()) || account.hasToken);
  const valid = entered
    ? Boolean(token.trim()) && Boolean(enteredId) === Boolean(enteredSecret)
    : account.hasToken || account.state === "unvalidated";
  const windowsMessage = isMac ? "" : message;
  useEffect(() => {
    if (account.state === "missing" || (account.state === "ready" && (isMac || windowsMessage.startsWith("Signed in")))) {
      setToken("");
      setAppId("");
      setSecret("");
      resetMask(revision => revision + 1);
      setManual(false);
      if (isMac) setTokenFeedback(undefined);
    }
  }, [account.state, commitRevision, windowsMessage]);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (disabled) return;
    if (byEmail) {
      signIn();
      return;
    }
    const input = entered ? { token: token.trim(), appId: enteredId, secret: enteredSecret } : undefined;
    if (!valid) return;
    const saved = await validate(input);
    if (!isMac && saved) { setToken(""); setAppId(""); setSecret(""); }
  }

  return (
    <>
      <MovingChoices className="mode-picker account-method" role="group" aria-label="Qobuz sign-in method" value={method}>
        {(["token", "email"] as const).map(choice => (
          <Button key={choice} data-choice={choice} aria-pressed={method === choice} disabled={disabled}
            onClick={() => {
              if (choice === method) return;
              if (isMac && !byEmail && message) setTokenFeedback(errorCode ? { message, code: errorCode } : undefined);
              setMethod(choice);
              clearMessage();
            }}>
            {choice === "token" ? "Token" : "Email & password"}
          </Button>
        ))}
      </MovingChoices>
      <form onSubmit={submit}>
        {byEmail ? <p className="dim">Enter your email and password on the Qobuz page in the sign-in window.</p> : (
          <>
            <SecretField key={`token-${maskRevision}`} label="Qobuz token" value={token}
              change={setToken} disabled={disabled} placeholder={account.hasToken ? "••••••••" : undefined} />
            <Button className="ghost" aria-expanded={manual} disabled={disabled} onClick={() => setManual(open => !open)}>
              Enter app credentials manually
            </Button>
            {manual && <div className="app-credentials">
              <Field label="App ID">
                <input autoComplete="off" disabled={disabled} value={appId}
                  placeholder={account.hasAppId ? "Saved" : undefined} onChange={event => setAppId(event.target.value)} />
              </Field>
              <SecretField key={`secret-${maskRevision}`} label="App secret" value={secret}
                change={setSecret} disabled={disabled} placeholder={account.hasSecret ? "••••••••" : undefined} />
              {isMac && <p className="dim">suonelo saves a validated manual pair for future token sign-ins on this profile.</p>}
              {entered && !valid && <p className={isMac ? "notice error" : "notice"} role={isMac ? "status" : undefined}>Enter the token and both app fields, or clear both app fields.</p>}
            </div>}
            {isMac && account.hasRememberedAppPair && <p className="dim">Saved app credentials are available if Qobuz rejects the Web Player App ID.</p>}
          </>
        )}
        <div className="actions">
          {isMac && !byEmail && !entered && account.canRememberSavedAppPair && <Button disabled={disabled}
            onClick={() => void validate({ token: "", rememberSavedAppPair: true })}>Remember saved app credentials</Button>}
          {!byEmail && canRetry && <Button disabled={disabled}
            onClick={() => void validate(automaticSetupRetry(token.trim()))}>Retry automatic setup</Button>}
          <ValidationButton className="primary" type="submit"
            disabled={disabled || (!byEmail && (!valid || (!entered && account.state === "rejected")))}
            label={byEmail ? "Sign in" : !entered && account.state === "unvalidated" ? "Retry validation" : "Validate"}
            validating={validating} />
          <Button className="danger" disabled={disabled || !(account.hasSavedCredentials ?? account.hasToken)} onClick={remove}>
            Remove credentials
          </Button>
        </div>
        {busy && <Button className="ghost" onClick={cancel}>{byEmail ? "Cancel sign-in" : "Cancel validation"}</Button>}
        <p className={feedback ? "notice" : "dim account-state"} role="status">
          {feedback || (account.state === "ready" && account.accessMode === "preview" ? "Signed in for MP3 previews." : accountLabels[account.state])}
        </p>
      </form>
    </>
  );
}

export interface AccountActions {
  validate: (input?: Credentials) => Promise<boolean>;
  signIn: () => void;
  remove: () => void;
  cancel: () => void;
  clearMessage: () => void;
  busy: boolean;
  validating: boolean;
  message: string;
  errorCode?: string | null;
  commitRevision: number;
}

export function SettingsPage({
  section, select, settings, patch, theme, retryTheme, metadata, account, accountActions, pending, apply, notify, modePending,
}: {
  section: Section; select: (section: Section) => void; settings: Settings; patch: Edit; theme: Theme; retryTheme?: () => void;
  metadata: MetadataField[]; account: Account;
  accountActions: AccountActions; pending: boolean; apply: () => void; notify: Notify; modePending: boolean;
}) {
  return (
    <div className="settings">
      <nav aria-label="Settings sections">
        {sections.map(name => <Button key={name} aria-current={section === name} onClick={() => select(name)}>{name}</Button>)}
      </nav>
      <section className={section === "Privacy" ? "config panel privacy-panel" : "config panel"} aria-labelledby="section-title">
        <h2 id="section-title">{section === "Privacy" ? "Privacy Policy" : section}</h2>
        {section === "Privacy" && <PrivacyPolicy notify={notify} />}
        {descriptions[section] && <p>{descriptions[section]}</p>}
        {section === "Download" && <DownloadSettings settings={settings} patch={patch} pending={pending} apply={apply} modePending={modePending} />}
        {section === "Output" && <OutputSettings settings={settings} patch={patch} notify={notify} />}
        {section === "Artwork & Metadata" && <ArtworkMetadata settings={settings} patch={patch} metadata={metadata} />}
        {section === "Account" && <AccountSettings account={account} {...accountActions} />}
        {section === "Appearance" && (
          <>
            <p className="field-title" id="color-mode">Color mode</p>
            <MovingChoices className="mode-picker" role="group" aria-labelledby="color-mode" value={settings.appearance}>
              {modes.map(({ id, label, Icon }) => (
                <Button key={id} data-choice={id} aria-pressed={settings.appearance === id} onClick={() => animateAppearance(() => patch({ appearance: id }))}>
                  <Icon className="icon" aria-hidden /> {label}
                </Button>
              ))}
            </MovingChoices>
            <p className="field-title" id="theme-list">Themes</p>
            <div className="themes" role="group" aria-labelledby="theme-list">
              {(Object.entries(themes) as [Theme, string][]).map(([id, label]) => (
                <Button key={id} className="theme" aria-pressed={theme === id}
                  onClick={() => animateAppearance(() => patch({ theme: id }))}>
                  <span className="swatch" data-theme={id} aria-hidden />{label}
                </Button>
              ))}
            </div>
            {retryTheme && <Button onClick={retryTheme}>Retry saving theme</Button>}
          </>
        )}
        {section === "About" && <About notify={notify} />}
      </section>
    </div>
  );
}
