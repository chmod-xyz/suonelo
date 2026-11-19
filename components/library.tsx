import { isMac } from "@/lib/platform";
import { Bookmark as BookmarkIcon, ChevronRight, Download, FolderOpen, Plus, RotateCw, Trash2, UserRound, X } from "lucide-react";
import { Fragment, useEffect, useId, useRef, useState } from "react";
import { desktop, type Bookmark, type BookmarkScope, type FileItem, type Job } from "@/lib/desktop";
import { Button, interactiveTarget, Progress } from "./ui/primitives";
import { ReleaseDetails } from "./ui/release-details";
import { RollingPercent } from "./ui/rolling-percent";

export const filters = ["Show all", "Complete", "Downloading", "Failed", "Incomplete", "Queued"] as const;
export type Filter = typeof filters[number];

function primaryFiles(job: Job) {
  const kind = job.mode === "booklet" ? "booklet" : "audio";
  return job.files.filter(file => file.kind === kind);
}

export function matches(job: Job, filter: Filter) {
  if (filter === "Complete") return job.status === "completed";
  if (filter === "Show all" || filter.toLowerCase() === job.status) return true;
  if (filter === "Downloading" && job.status === "retrying") return true;
  if (filter !== "Incomplete" || job.status !== "cancelled") return false;
  const files = primaryFiles(job);
  return files.some(file => file.status === "completed") && files.some(file => file.status !== "completed");
}

const labels = {
  queued: "Queued", downloading: "Downloading", completed: "Completed",
  failed: "Failed", incomplete: "Incomplete", cancelled: "Cancelled",
  retrying: "Retrying", unavailable: "Unavailable",
};

const kindLabels = { artwork: "Cover", booklet: "Booklet", lyrics: "Lyrics", audio: "Track", playlist: "Playlist" };

export function AddLinkButton({ onClick }: { onClick: () => void }) {
  return (
    <Button className="primary with-icon" data-action="add" aria-keyshortcuts={isMac ? "Meta+N" : "Control+N"} onClick={onClick}>
      <Plus className="icon" aria-hidden /> Add link <kbd>{isMac ? "⌘N" : "Ctrl+N"}</kbd>
    </Button>
  );
}

function Release({ title, subtitle, url, thumbnail, resourceKind }: {
  title: string; subtitle: string; url: string; thumbnail?: string | null; resourceKind?: string;
}) {
  const [failedImage, setFailedImage] = useState<string | null>(null);
  const artist = resourceKind === "artist";
  const image = thumbnail?.startsWith("data:image/") && thumbnail !== failedImage ? thumbnail : null;
  const coverClass = `cover${artist ? " artist-cover" : ""}`;
  return (
    <>
      {image ? <img className={coverClass} src={image} alt="" width={40} height={40} onError={() => setFailedImage(image)} />
        : <span className={`${coverClass} placeholder-cover`} aria-hidden>
          {artist ? <UserRound className="icon" /> : <BookmarkIcon className="icon" />}
        </span>}
      <div className="release-copy">
        <strong title={title}>{title || url}</strong>
        <small title={url}>{subtitle || url}</small>
      </div>
    </>
  );
}

function fileSize(file: FileItem) {
  if (file.finalSizeBytes != null) return `${(file.finalSizeBytes / 1_000_000).toFixed(1)} MB`;
  if (file.totalBytes != null) return `${(file.receivedBytes / 1_000_000).toFixed(1)} / ${(file.totalBytes / 1_000_000).toFixed(1)} MB`;
  return file.receivedBytes > 0 ? `${(file.receivedBytes / 1_000_000).toFixed(1)} MB / unknown` : "—";
}

function fileStatus(file: FileItem) {
  if (file.phase === "processing") return "Processing";
  if (file.phase === "connecting") return "Connecting";
  if (file.phase === "transferring") return file.progress === null ? "Unknown total" : <RollingPercent value={file.progress} />;
  if (file.kind === "lyrics" && (file.status === "unavailable" || file.status === "failed")) return `Lyrics ${file.status}`;
  return labels[file.status];
}

function Files({ files, bookletOnly, canRetry, retry, openFolder, replace, paused }: {
  files: FileItem[]; bookletOnly: boolean; canRetry: boolean; retry: (id: number) => void; openFolder: (id: number) => void;
  replace: (file: FileItem) => void; paused: boolean;
}) {
  let lastGroup: string | null = null;
  return files.map(file => {
    const primary = file.kind === (bookletOnly ? "booklet" : "audio");
    const disc = `Disc ${file.disc ?? 1}`;
    const group = bookletOnly ? file.album : [file.album, disc].filter(Boolean).join(" · ");
    const showGroup = primary && group && group !== lastGroup;
    if (primary) lastGroup = group ?? null;
    const transferring = file.phase === "transferring" || file.phase === "connecting";
    return (
      <Fragment key={file.id}>
        {showGroup && <div className="disc">{group}</div>}
        <div className="track" title={file.conflictPath ?? file.path ?? undefined}>
          <div className="file-copy">
            {file.kind !== "audio" && `${kindLabels[file.kind]} · `}{file.title}
            {file.quality && <span className="quality"> · {file.quality}</span>}
            {file.reused && <span className="quality"> · Verified existing file</span>}
            {file.reason && !(file.kind === "lyrics" && file.status === "unavailable") && (
              <div className={primary && file.status === "failed" ? "notice error" : "notice"}>{file.reason}</div>
            )}
            {file.conflictPath && <div className="conflict-path"><code>{file.conflictPath}</code></div>}
            {file.status === "downloading" && <Progress value={file.progress ?? null} active={transferring} paused={paused}
              label={`${file.title} ${file.phase || "transfer"}`} />}
          </div>
          <span className="file-status">
            <span className="file-info">
              <span className="file-size">{fileSize(file)}</span>
              <span className={`status ${file.status}`}>{fileStatus(file)}</span>
            </span>
            {file.conflictPath && (
              <Button className="with-icon" onClick={() => openFolder(file.id)}>
                <FolderOpen className="icon" aria-hidden /> Open containing folder
              </Button>
            )}
            {file.status === "failed" && file.independentRetry && (
              <Button className="with-icon" disabled={!canRetry} onClick={() => retry(file.id)}>
                <RotateCw className="icon" aria-hidden /> Retry {kindLabels[file.kind].toLowerCase()}
              </Button>
            )}
            {file.status === "failed" && file.conflictPath && file.replacementToken && (
              <Button className="danger" disabled={!canRetry} onClick={() => replace(file)}>Replace saved audio</Button>
            )}
          </span>
        </div>
      </Fragment>
    );
  });
}

export interface JobActions {
  retry: (id: number) => void;
  retryFile: (jobId: number, fileId: number) => void;
  replaceFile: (jobId: number, file: FileItem) => void;
  openFolder: (jobId: number, fileId: number) => void;
  openReleaseFolder: (id: number) => Promise<void>;
  cancel: (id: number) => void;
  remove: (id: number) => void;
}

function ReleaseFolderButton({ job, open }: { job: Job; open: () => Promise<void> }) {
  const help = useId();
  const [refresh, setRefresh] = useState(0);
  const [status, setStatus] = useState({ available: false, reason: "Checking the saved release folder." as string | null });
  const evidence = JSON.stringify([job.id, job.generation, job.status, job.files.map(file => [file.id, file.status, file.path])]);
  useEffect(() => {
    let active = true;
    setStatus({ available: false, reason: "Checking the saved release folder." });
    void desktop.releaseFolderStatus(job.id).then(result => { if (active) setStatus(result); })
      .catch(() => { if (active) setStatus({ available: false, reason: "The saved release folder could not be checked." }); });
    return () => { active = false; };
  }, [evidence, refresh, job.id]);
  return <span title={status.reason ?? undefined}>
    <Button className="with-icon" style={!status.available ? { opacity: .45, cursor: "default" } : undefined} onFocus={() => { if (!status.available) setRefresh(value => value + 1); }} aria-disabled={!status.available} aria-describedby={!status.available ? help : undefined}
      onClick={() => { if (status.available) void open().finally(() => setRefresh(value => value + 1)); }}>
      <FolderOpen className="icon" aria-hidden /> Open folder
    </Button>
    {!status.available && <span id={help} hidden>{status.reason}</span>}
  </span>;
}

function QueueEntry({ job, actions, open, toggle, paused }: {
  job: Job; actions: JobActions; open: boolean; toggle: () => void; paused: boolean;
}) {
  const details = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const files = primaryFiles(job);
  const completed = files.filter(file => file.status === "completed");
  const formats = Array.from(new Set(completed.flatMap(file => file.quality ? [file.quality] : [])));
  const downloading = job.status === "downloading" || job.status === "retrying";
  const active = downloading || job.status === "queued";
  const releaseProgress = job.progress ?? 0;
  function toggleDetails() {
    if (open && document.getElementById(details)?.contains(document.activeElement)) trigger.current?.focus();
    toggle();
  }
  const [command, label] = active
    ? [actions.cancel, "Cancel"]
    : [actions.retry, "Retry"];
  return (
    <article className={open ? "entry open" : "entry"}>
        <div className="job" onClick={event => {
          if (!interactiveTarget(event.target)) toggleDetails();
        }}>
          <div className="release">
              <Button ref={trigger} className="ghost chevron" aria-expanded={open} aria-controls={details} onClick={toggleDetails}
                aria-label={`File details for ${job.title || job.url}`}>
                <ChevronRight className="icon" aria-hidden />
              </Button>
            <Release title={job.title} subtitle={job.subtitle} url={job.url} thumbnail={job.thumbnail} resourceKind={job.resourceKind} />
          </div>
          <div>
            <span className={`status ${job.status}`} title={job.reason ?? undefined}>
              {downloading ? <RollingPercent value={releaseProgress} /> : labels[job.status]}
            </span>
            {job.reason && <span className="row-reason" title={job.reason}>{job.reason}</span>}
            {active && <Progress value={releaseProgress} label="Release download progress" active={downloading} paused={paused} />}
          </div>
          <div className="qcol quality">
            {job.mode === "booklet" ? "PDF" : formats.length === 1 ? formats[0] : formats.length > 1 ? "Mixed quality" : "—"}
            {files.length > 0 && ` · ${completed.length}/${files.length}${job.mode === "booklet" ? "" : " tracks"}`}
          </div>
          <div className="actions">
            {job.driveBlocked && active && (
              <Button className="with-icon" onClick={() => actions.retry(job.id)}>
                <RotateCw className="icon" aria-hidden /> Retry
              </Button>
            )}
            {(job.status === "completed" || job.status === "incomplete") &&
              <ReleaseFolderButton job={job} open={() => actions.openReleaseFolder(job.id)} />}
            {job.status !== "completed" && <Button className="with-icon" onClick={() => command(job.id)}>
              {active ? <X className="icon" aria-hidden /> : <RotateCw className="icon" aria-hidden />} {label}
            </Button>}
            <Button className="ghost with-icon" onClick={() => actions.remove(job.id)}>
              <Trash2 className="icon" aria-hidden /> Remove
            </Button>
          </div>
        </div>
        <ReleaseDetails id={details} open={open}>
          {job.reason && <div className={job.status === "failed" ? "notice error" : "notice"}>{job.reason}</div>}
          <Files files={job.files} paused={paused} bookletOnly={job.mode === "booklet"} canRetry={!active} retry={fileId => actions.retryFile(job.id, fileId)}
            replace={file => actions.replaceFile(job.id, file)}
            openFolder={fileId => actions.openFolder(job.id, fileId)} />
          {!job.reason && job.files.length === 0 && <p className="notice">Waiting for the file plan.</p>}
        </ReleaseDetails>
    </article>
  );
}

export function Downloads({ jobs, filter, actions, expanded, toggle, paused }: {
  jobs: Job[]; filter: Filter; actions: JobActions; expanded: Set<number>; toggle: (id: number) => void; paused: boolean;
}) {
  const shown = jobs.filter(job => matches(job, filter));
  return (
    <section className="panel" aria-label="Download queue">
      <div className="columns"><span>Release</span><span>Status</span><span className="qcol">Format / Files</span><span>Actions</span></div>
      {shown.map(job => <QueueEntry key={job.id} job={job} actions={actions} open={expanded.has(job.id)} paused={paused} toggle={() => toggle(job.id)} />)}
      {shown.length === 0 && <p className="empty">{filter === "Show all" ? "No downloads." : filter === "Complete" ? "No completed downloads." : `No ${filter.toLowerCase()} jobs.`}</p>}
    </section>
  );
}

export function Bookmarks({
  bookmarks, selected, toggle, deselect, download, remove, clear, retry,
}: {
  bookmarks: Bookmark[];
  selected: Set<number>;
  toggle: (id: number) => void;
  deselect: () => void;
  download: (scope: BookmarkScope) => void;
  remove: (ids: number[]) => void;
  clear: () => void;
  retry: (id: number) => void;
}) {
  return (
    <section className="panel" aria-label="Saved links">
      <div className="toolbar">
        <Button className="with-icon" disabled={!selected.size} onClick={() => download({ kind: "selected", ids: [...selected] })}>
          <Download className="icon" aria-hidden /> Download selected
        </Button>
        <Button className="with-icon" disabled={!selected.size} onClick={() => remove([...selected])}>
          <Trash2 className="icon" aria-hidden /> Remove selected
        </Button>
        <Button className="with-icon" disabled={!bookmarks.length} onClick={() => download({ kind: "all" })}>
          <Download className="icon" aria-hidden /> Download all
        </Button>
        <Button className="ghost" disabled={!selected.size} onClick={deselect}>Deselect all</Button>
        <Button className="danger with-icon" disabled={!bookmarks.length} onClick={clear}>
          <Trash2 className="icon" aria-hidden /> Clear all
        </Button>
      </div>
      {bookmarks.map(bookmark => {
        const name = bookmark.albumName || bookmark.title || bookmark.url;
        const depth = bookmark.bitDepth ? `${bookmark.bitDepth}-bit` : "";
        const rate = bookmark.samplingRate
          ? `${bookmark.samplingRate >= 1000 ? bookmark.samplingRate / 1000 : bookmark.samplingRate} kHz` : "";
        return (
          <article className="bookmark" key={bookmark.id} onClick={event => {
            if (!interactiveTarget(event.target)) toggle(bookmark.id);
          }}>
            <input type="checkbox" aria-label={`Select ${name}`} checked={selected.has(bookmark.id)} onChange={() => toggle(bookmark.id)} />
            <div className="release">
              <Release title={name} subtitle={[bookmark.albumArtist, bookmark.url].filter(Boolean).join(" · ")} url={bookmark.url}
                thumbnail={bookmark.thumbnail} resourceKind={bookmark.resourceKind} />
            </div>
            <span className="quality">{[depth, rate].filter(Boolean).join(" / ") || "—"}</span>
            <Button className="with-icon" onClick={() => download({ kind: "single", id: bookmark.id })}>
              <Download className="icon" aria-hidden /> Download
            </Button>
            {bookmark.reason && (
              <div className="bookmark-warning notice">
                <span>{bookmark.reason}</span>
                <Button className="with-icon" onClick={() => retry(bookmark.id)}><RotateCw className="icon" aria-hidden /> Retry</Button>
              </div>
            )}
          </article>
        );
      })}
      {!bookmarks.length && <p className="empty">No saved links.</p>}
    </section>
  );
}
