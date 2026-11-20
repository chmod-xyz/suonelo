import { Channel, invoke } from "@tauri-apps/api/core";

export type Mode = "system" | "light" | "dark";
export type DownloadMode = "sequential" | "concurrent";
export type ConnectionLimit = "auto" | "unlimited" | 1 | 2 | 4 | 8 | 16 | 32;
export type Theme = "Suonelo" | "Vercel" | "Spotify" | "Material Design" | "Claymorphism" | "Violet";

export interface Settings {
  revision: number;
  quality: number;
  intervalSeconds: number;
  maxConnections: ConnectionLimit;
  downloadMode: DownloadMode;
  limitedToAlbumReleases: boolean;
  allowFallback: boolean;
  bookletOnly: boolean;
  createPlaylist: boolean;
  smartDiscography: boolean;
  safeMode: boolean;
  folderTemplate: string;
  filenameTemplate: string;
  lyricsMode: "off" | "embed" | "external";
  artwork: "original" | "1200" | "600" | "300";
  downloadCoverArt: boolean;
  embedCoverArt: boolean;
  coverFilename: string;
  tags: Record<string, boolean>;
  outputPath: string;
  appearance: Mode;
  theme: Theme | null;
  lightTheme?: Theme | null;
  darkTheme?: Theme | null;
  outputMissing: boolean;
}

export type SettingsPatch = Partial<Omit<Settings, "revision" | "outputMissing" | "lightTheme" | "darkTheme" | "theme">> & { theme?: Theme };
export type FileStatus = "queued" | "downloading" | "retrying" | "completed" | "failed" | "cancelled" | "unavailable";
export type FilePhase = "queued" | "connecting" | "transferring" | "processing" | "retrying" | "completed" | "failed" | "cancelled" | "unavailable";

export interface FileItem {
  id: number;
  kind: "audio" | "artwork" | "booklet" | "lyrics" | "playlist";
  title: string;
  album?: string | null;
  disc?: number | null;
  status: FileStatus;
  generation: number;
  attempt: number;
  progress: number | null;
  phase: FilePhase;
  receivedBytes: number;
  totalBytes: number | null;
  finalSizeBytes: number | null;
  quality?: string | null;
  path?: string | null;
  reason?: string | null;
  conflictPath?: string | null;
  replacementToken?: string | null;
  reused?: boolean;
  independentRetry?: boolean;
}

export interface Job {
  id: number;
  url: string;
  title: string;
  subtitle: string;
  status: FileStatus | "incomplete";
  mode?: "audio" | "booklet";
  generation: number;
  progress: number | null;
  clearable: boolean;
  thumbnail?: string | null;
  resourceKind?: string;
  driveBlocked?: boolean;
  canRetry?: boolean;
  reason?: string | null;
  files: FileItem[];
}

export interface LargePlan {
  id: number;
  planRevision: number;
  trackCount: number;
  unit?: "tracks" | "pdfs";
}

export interface QueueSnapshot {
  revision: number;
  jobs: Job[];
  confirmations: LargePlan[];
  canClear: boolean;
  protectedCount: number;
  bytesPerSecond: number;
  effectiveMode: DownloadMode;
  requestedMode: DownloadMode;
  liveLimit: ConnectionLimit | null;
  paused: boolean;
  canPause: boolean;
  recovery?: {
    cooldownSeconds?: number | null;
    retryConnection?: boolean;
    workerCap?: number | null;
  };
}

export interface QueueProgress {
  revision: number;
  jobId: number;
  fileId: number;
  generation: number;
  attempt: number;
  progress: number | null;
  phase: FilePhase;
  receivedBytes: number;
  totalBytes: number | null;
  finalSizeBytes: number | null;
  jobProgress: number | null;
}

export interface QueueSpeed { revision: number; bytesPerSecond: number }
export type QueueEvent = QueueSnapshot | QueueProgress | QueueSpeed;

export interface Bookmark {
  id: number;
  url: string;
  title: string;
  albumArtist?: string | null;
  albumName?: string | null;
  bitDepth?: number | null;
  samplingRate?: number | null;
  reason?: string | null;
  thumbnail?: string | null;
  resourceKind?: string;
}

interface BookmarkSnapshot {
  revision: number;
  bookmarks: Bookmark[];
}

export interface Account {
  state: "missing" | "validating" | "unvalidated" | "ready" | "rejected";
  epoch?: number;
  hasToken: boolean;
  hasAppId: boolean;
  hasSecret: boolean;
  hasSavedCredentials?: boolean;
  hasRememberedAppPair?: boolean;
  canRememberSavedAppPair?: boolean;
  accessMode?: "unknown" | "full" | "preview";
  reasonCode?: string | null;
  reason?: string | null;
}

export interface Credentials {
  token: string;
  appId?: string;
  secret?: string;
  forceDiscovery?: boolean;
  rememberSavedAppPair?: boolean;
}

export interface MetadataField {
  id: string;
  label: string;
  defaultEnabled: boolean;
  eligible?: boolean;
  description?: string;
  group?: "album" | "track" | null;
  rawPath?: string | null;
  exportTag?: string | null;
}

export interface Bootstrap {
  settings: Settings;
  queue: QueueSnapshot;
  bookmarks: Bookmark[];
  account: Account;
}

export type Destination = "downloads" | "bookmarks";
export type SearchKind = "album" | "track" | "artist";
export type SearchSort = "relevance" | "name_asc" | "name_desc" | "date";
export type SearchAction = "start" | "filter" | "sort" | "more" | "retry";

export interface SearchId {
  kind: SearchKind;
  id: string;
}

export interface SearchGenre {
  id: number;
  label: string;
  parent: number | null;
  path: number[];
  storefrontPath?: number[];
}

export interface SearchRow {
  key: SearchId;
  title: string;
  credits: { id: string; name: string }[];
  byline: string | null;
  edition: string | null;
  date: string | null;
  genreId: number | null;
  bitDepth: number | null;
  sampleRate: number | null;
  hires: boolean | null;
  explicit: boolean | null;
  streamable: boolean | null;
  trackCount: number | null;
  disc: number | null;
  number: number | null;
  duration: number | null;
  release: string | null;
}

export interface SearchGroup {
  rows: SearchRow[];
  count: number;
  cursor: string | null;
  providerTotal: number | null;
  limited: boolean;
  error: { code: string; message: string; retryable: boolean } | null;
}

export interface SearchSnapshot {
  generation: number;
  groups: Record<SearchKind, SearchGroup>;
  history: number;
  historyError: SearchGroup["error"];
}

export interface SearchDetail {
  row: SearchRow;
  group: SearchGroup;
}

export interface SearchQuery {
  sequence: number;
  generation: number;
  query: string;
  genres: number[];
  sort: SearchSort;
  kind: SearchKind;
  action: SearchAction;
  cursor?: string | null;
}

export interface SearchSubmission {
  generation: number;
  category: SearchKind;
  detail?: SearchId;
  wholeRelease: boolean;
  selected: SearchId[];
  destination: Destination;
}

export interface SearchReview {
  token: string | null;
  count: number;
  skipped: SearchId[];
}

export interface SearchSubmissionReport {
  accepted: SearchId[];
  skipped: SearchId[];
  conflicts: SearchId[];
}
export type BookmarkScope = { kind: "all" } | { kind: "selected"; ids: number[] } | { kind: "single"; id: number };

export interface SubmissionReport {
  accepted: string[];
  duplicates: string[];
  invalid: { url: string; reason: string }[];
}

export interface LinkFileInspection {
  valid: boolean;
  filename: string;
  sizeBytes: number;
  encoding: "UTF-8";
  content: string | null;
  totalUrls: number;
  uniqueUrls: number;
  duplicates: number;
  invalidCount: number;
  invalid: { line: number; url: string; reason: string }[];
  reason?: string | null;
}

function subscribe<T>(command: string, receive: (snapshot: T) => void) {
  const channel = new Channel<T>(receive);
  return invoke<void>(command, { channel });
}

type ValidationResult = { account: Account | null; confirmation: "switch" | "active" | null };
type EmailSignInPhase = "waiting" | "validating";

export const desktop = {
  bootstrap: () => invoke<Bootstrap>("bootstrap"),
  queueSnapshot: () => invoke<QueueSnapshot>("queue_snapshot"),
  setQueuePaused: (paused: boolean) => invoke<QueueSnapshot>("set_queue_paused", { paused }),
  subscribeQueue: (receive: (event: QueueEvent) => void) => subscribe("subscribe_queue", receive),
  subscribeBookmarks: (receive: (snapshot: BookmarkSnapshot) => void) => subscribe("subscribe_bookmarks", receive),
  listBookmarks: () => invoke<Bookmark[]>("list_bookmarks"),
  submitLinks: (text: string, destination: Destination, qualityOverride?: number) =>
    invoke<SubmissionReport>("submit_links", { text, destination, qualityOverride }),
  inspectLinkFile: (path: string) => invoke<LinkFileInspection>("inspect_link_file", { path }),
  submitImportedLinks: (text: string, destination: Destination, qualityOverride?: number) =>
    invoke<SubmissionReport>("submit_imported_links", { text, destination, qualityOverride }),
  retryJob: (jobId: number) => invoke<void>("retry_job", { jobId }),
  retryFile: (jobId: number, fileId: number) => invoke<void>("retry_file", { jobId, fileId }),
  replaceSearchAudio: (jobId: number, fileId: number, token: string) => invoke<void>("replace_search_audio", { jobId, fileId, token }),
  openContainingFolder: (jobId: number, fileId: number) => invoke<void>("open_containing_folder", { jobId, fileId }),
  retryConnection: () => invoke<void>("retry_connection"),
  searchGenres: () => invoke<SearchGenre[]>("search_genres"),
  search: (request: SearchQuery) => invoke<SearchSnapshot>("search_query", { request }),
  cancelSearch: (sequence: number) => invoke<void>("search_cancel", { sequence }),
  searchDetail: (request: { sequence: number; generation: number; key: SearchId; cursor?: string | null; retry?: boolean }) =>
    invoke<SearchDetail>("search_detail", { request: { retry: false, ...request } }),
  searchArtwork: (generation: number, key: SearchId) => invoke<string | null>("search_artwork", { generation, key }),
  prepareSearch: (request: SearchSubmission) => invoke<SearchReview>("prepare_search_submission", { request }),
  commitSearch: (token: string) => invoke<SearchSubmissionReport>("commit_search_submission", { token }),
  clearSearchHistory: () => invoke<string>("clear_search_history"),
  releaseFolderStatus: (jobId: number) => invoke<{ available: boolean; reason: string | null }>("release_folder_status", { jobId }),
  openReleaseFolder: (jobId: number) => invoke<void>("open_release_folder", { jobId }),
  downloadAgain: (jobId: number) => invoke<void>("download_again", { jobId }),
  cancelJob: (jobId: number) => invoke<void>("cancel_job", { jobId }),
  removeJob: (jobId: number) => invoke<void>("remove_job", { jobId }),
  clearDownloads: () => invoke<{ removed: number; protected: number }>("clear_downloads"),
  applySettingsToPending: (ids: number[]) => invoke<void>("apply_settings_to_pending", { ids }),
  decideLargeSubmission: (plan: LargePlan, decision: boolean) => invoke<void>("decide_large_submission", {
    id: plan.id, planRevision: plan.planRevision, decision,
  }),
  downloadBookmarks: (scope: BookmarkScope) => invoke<SubmissionReport>("download_bookmarks", { scope }),
  retryBookmark: (id: number) => invoke<void>("retry_bookmark", { id }),
  removeBookmarks: (ids: number[]) => invoke<void>("remove_bookmarks", { ids }),
  clearBookmarks: () => invoke<void>("clear_bookmarks"),
  readSettings: () => invoke<Settings>("read_settings"),
  patchSettings: (expectedRevision: number, patch: SettingsPatch) =>
    invoke<Settings>("patch_settings", { expectedRevision, patch }),
  saveConnectionLimit: (expectedRevision: number, limit: ConnectionLimit) =>
    invoke<{ settings: Settings; queue: QueueSnapshot }>("save_connection_limit", { expectedRevision, limit }),
  migrateTheme: (expectedRevision: number, legacyDark: boolean) =>
    invoke<Settings>("migrate_theme", { expectedRevision, legacyDark }),
  resetSettings: (expectedRevision: number) => invoke<Settings>("reset_settings", { expectedRevision }),
  removeCredentials: () => invoke<Account>("reset_vault", { confirmation: true }),
  validateCredentials: (attempt: string, input?: Credentials) =>
    invoke<ValidationResult>("validate_credentials", { attempt, input }),
  startEmailSignIn: (attempt: string, receive: (phase: EmailSignInPhase) => void) =>
    invoke<ValidationResult>("start_email_sign_in", { attempt, phase: new Channel(receive) }),
  confirmAccountValidation: (attempt: string) => invoke<Account>("confirm_account_validation", { attempt }),
  cancelAccountValidation: (attempt: string) => invoke<void>("cancel_account_validation", { attempt }),
  metadataCatalog: () => invoke<MetadataField[]>("metadata_catalog"),
  previewOutput: (folder: string, filename: string, quality: number, safeMode?: boolean) =>
    invoke<{ path: string }>("preview_output", { template: { folder, filename }, quality, safeMode }),
  openCredits: () => invoke<void>("open_credits"),
  openAboutLink: (link: "project" | "qobuz-privacy") => invoke<void>("open_about_link", { link }),
  requestClose: () => invoke<{ requiresConfirmation: boolean }>("request_close"),
  shutdownSession: () => invoke<void>("shutdown_session"),
};

export function automaticSetupFailure(code?: string | null): boolean {
  return code === "credential_discovery" || code === "invalid_app_id" || code === "account_rejected";
}

export function errorCode(error: unknown): string | undefined {
  if (typeof error === "object" && error !== null && "code" in error && typeof error.code === "string") return error.code;
}

export function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error && typeof error === "object" && "message" in error && typeof error.message === "string") {
    return error.message;
  }
  return "The desktop operation could not be completed.";
}
