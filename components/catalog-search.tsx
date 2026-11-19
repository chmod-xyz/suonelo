import { ArrowLeft, ArrowRight, Bookmark, ChevronDown, Disc3, Download, Grid2X2, List, Music, Search, X } from "lucide-react";
import { useEffect, useRef, useState, type ChangeEvent } from "react";
import { desktop, type Account, type Destination, type SearchGenre, type SearchId, type SearchRow, type SearchSort, type SearchSubmissionReport } from "@/lib/desktop";
import { searchKey, useCatalogSearch } from "@/lib/use-catalog-search";
import { Modal } from "./dialogs";
import { Button } from "./ui/primitives";
import { AnimatedSelect } from "./ui/animated-select";
import { MovingChoices } from "./ui/moving-choices";
import { usePopupStagger } from "./ui/use-popup-stagger";

const tabs = [{ kind: "album", label: "Releases" }, { kind: "track", label: "Tracks" }, { kind: "artist", label: "Artists" }] as const;
const sortOptions = [
  { value: "relevance", label: "Relevance" }, { value: "name_asc", label: "Name A–Z" },
  { value: "name_desc", label: "Name Z–A" }, { value: "date", label: "Release date" },
] as const satisfies readonly { value: SearchSort; label: string }[];

function GenreBadges({ genres, labels, selected, change, open }: {
  genres: SearchGenre[]; labels: Map<number, string>; selected: number[]; change: (ids: number[]) => void; open: boolean;
}) {
  const list = useRef<HTMLDivElement>(null);
  usePopupStagger(list, "button", open);
  return <div ref={list} className="search-genre-badges" role="group" aria-label="Genres">
    {genres.map(genre => <Button key={genre.id} aria-pressed={selected.includes(genre.id)}
      title={(genre.storefrontPath ?? genre.path).slice().reverse().map(id => labels.get(id)).filter(Boolean).join(" › ")}
      onClick={() => change(selected.includes(genre.id)
        ? selected.filter(id => id !== genre.id) : [...selected, genre.id])}>{genre.label}</Button>)}
    {!genres.length && <p className="dim" role="status">No matching genres.</p>}
  </div>;
}

function Artwork({ row, generation }: { row: SearchRow; generation: number }) {
  const ref = useRef<HTMLSpanElement>(null);
  const [image, setImage] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    const observer = new IntersectionObserver(entries => {
      if (!entries.some(entry => entry.isIntersecting)) return;
      observer.disconnect();
      void desktop.searchArtwork(generation, row.key).then(url => {
        if (current) setImage(url);
      }).catch(() => undefined);
    }, { rootMargin: "80px" });
    if (ref.current) observer.observe(ref.current);
    return () => { current = false; observer.disconnect(); };
  }, [generation, row.key.kind, row.key.id]);
  return <span className="search-art" ref={ref} aria-hidden>
    {image ? <img src={image} alt="" /> : row.key.kind === "artist"
      ? <span className="search-initials">{row.title.split(/\s+/).slice(0, 2).map(word => Array.from(word)[0]).join("")}</span>
      : <Disc3 />}
  </span>;
}

function Quality({ row }: { row: SearchRow }) {
  const specs = [row.bitDepth ? `${row.bitDepth}-bit` : null, row.sampleRate ? `${row.sampleRate} kHz` : null].filter(Boolean).join(" / ");
  if (!specs && !row.hires && !row.explicit) return null;
  return <span className="search-quality">
    {specs && <span>{specs}</span>}
    {(row.hires || row.explicit) && <span className="search-badges">
      {row.hires && <span className="search-hires"><Music /> Hi-Res</span>}
      {row.explicit && <span>Explicit</span>}
    </span>}
  </span>;
}

function Checkbox({ label, checked, mixed, disabled, change }: {
  label: string; checked: boolean; mixed?: boolean; disabled?: boolean; change: (event: ChangeEvent<HTMLInputElement>) => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => { if (ref.current) ref.current.indeterminate = Boolean(mixed); }, [mixed]);
  return <input ref={ref} type="checkbox" aria-label={label} checked={checked} disabled={disabled} onChange={change} />;
}

function ResultRow({ row, generation, checked, disabled, numbered, genre, previewOnly, toggle, open }: {
  row: SearchRow; generation: number; checked: boolean; disabled: boolean; numbered: boolean; genre?: string;
  previewOnly?: boolean;
  toggle: (on: boolean) => void; open: (key: SearchId) => void;
}) {
  const artist = row.key.kind === "artist";
  const track = row.key.kind === "track";
  const release = row.release;
  const title = row.title;
  const edition = row.edition && !row.title.includes(row.edition) ? row.edition : null;
  const byline = row.credits.map(credit => credit.name).join(", ") || row.byline;
  const duration = row.duration == null ? "" : `${Math.floor(row.duration / 60)}:${String(row.duration % 60).padStart(2, "0")}`;
  const meta = [edition, row.date?.slice(0, 4), genre, row.trackCount == null ? "" : `${row.trackCount} tracks`, track ? duration : ""].filter(Boolean).join(" · ");
  return <li className="search-row" data-search-key={searchKey(row.key)} data-track={track || undefined} data-checked={checked || undefined}>
    {numbered && <span className="search-track-number">{row.number ?? "–"}</span>}
    <button type="button" className="search-result-main" aria-pressed={track ? checked : undefined}
      disabled={track && disabled} onClick={() => track ? toggle(!checked) : open(row.key)}>
      {!numbered && <Artwork key={generation} row={row} generation={generation} />}
      <span className="search-copy">
        <strong title={title}>{title}</strong>
        {byline && <small title={byline}>{byline}</small>}
        {meta && <small title={meta} className="search-meta">{meta}</small>}
        {!previewOnly && !artist && row.streamable === false && <small className="search-meta">Unavailable for this account</small>}
        <Quality row={row} />
      </span>
    </button>
    {track && !numbered && release && <Button className="search-open-release" title="Open release"
      aria-label={`Open release for ${title}`} onClick={() => open({ kind: "album", id: release })}><ArrowRight className="icon" /></Button>}
    {!artist && <label className="search-check">
      <Checkbox label={`Select ${title}`} checked={checked} disabled={disabled} change={event => toggle(event.target.checked)} />
    </label>}
  </li>;
}

function LoadingRows({ layout }: { layout: "cards" | "table" }) {
  return <>
    <p className="search-status" role="status">Searching Qobuz…</p>
    <ul className={`search-results ${layout}`} aria-hidden>
      {Array.from({ length: layout === "cards" ? 5 : 7 }, (_, index) =>
        <li key={index} className="search-row search-skeleton">
          <span className="search-art" />
          <span className="search-copy"><span /><span /></span>
        </li>)}
    </ul>
  </>;
}

export function CatalogSearch({ open, close, account, beforeQueue, onSubmit }: {
  open: boolean; close: () => void; account?: Account; beforeQueue: () => Promise<void>;
  onSubmit: (report: SearchSubmissionReport, destination: Destination) => void;
}) {
  const search = useCatalogSearch(open, account, beforeQueue, onSubmit);
  const [leaving, setLeaving] = useState(false);
  const dialog = useRef<HTMLDialogElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const scroll = useRef<HTMLDivElement>(null);
  const [filterOpen, setFilterOpen] = useState(false);
  const [genreDraft, setGenreDraft] = useState("");
  const [genreTerm, setGenreTerm] = useState("");
  const genreTrigger = useRef<HTMLButtonElement>(null);
  const genreInput = useRef<HTMLInputElement>(null);
  const anchor = useRef<{ key: string; top: number }>(undefined);
  const mounted = useRef(true);
  const group = search.group;
  const detail = search.frame?.value;
  const numbered = search.frame?.key.kind === "album";
  const layout = numbered ? "table" : search.layout;
  const generation = search.snapshot?.generation ?? 0;
  const anyGenre = !search.filter.length || search.filter.length === search.genres.length;
  const selectedGenre = search.genres.find(genre => genre.id === search.filter[0]);
  const labels = new Map(search.genres.map(genre => [genre.id, genre.label]));
  const canSubmit = !search.pending && !search.writing && search.chosen.length > 0;
  const genreDisabled = !search.draft.trim() || !search.genres.length || search.writing;
  const visibleGenres = genreTerm ? search.genres.filter(genre => genre.label.toLowerCase().includes(genreTerm)) : search.genres;

  useEffect(() => {
    if (!filterOpen) return;
    const timer = setTimeout(() => setGenreTerm(genreDraft.trim().toLowerCase()), 250);
    return () => clearTimeout(timer);
  }, [filterOpen, genreDraft]);

  function showGenres(next: boolean) {
    setFilterOpen(next);
    if (!next) {
      setGenreDraft("");
      setGenreTerm("");
    }
  }

  useEffect(() => {
    if (!open || genreDisabled) showGenres(false);
  }, [open, genreDisabled]);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useEffect(() => {
    const saved = anchor.current;
    if (!saved || !scroll.current || search.pending) return;
    const row = Array.from(scroll.current.querySelectorAll<HTMLElement>("[data-search-key]")).find(row => row.dataset.searchKey === saved.key);
    if (row) scroll.current.scrollTop += row.getBoundingClientRect().top - saved.top;
    anchor.current = undefined;
  }, [group, search.pending]);

  useEffect(() => {
    if (scroll.current) scroll.current.scrollTop = 0;
  }, [search.kind, search.frame?.key.id, generation, layout]);

  function loadMore() {
    const box = scroll.current;
    const top = box?.getBoundingClientRect().top;
    const row = top === undefined ? undefined : Array.from(box!.querySelectorAll<HTMLElement>("[data-search-key]"))
      .find(row => row.getBoundingClientRect().bottom > top);
    if (row?.dataset.searchKey) anchor.current = { key: row.dataset.searchKey, top: row.getBoundingClientRect().top };
    search.more();
  }

  async function dismiss(closeFilterFirst = false) {
    if (leaving || search.writing) return;
    if (closeFilterFirst && filterOpen) {
      showGenres(false);
      genreTrigger.current?.focus();
      return;
    }
    search.interrupt();
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      close();
      return;
    }
    setLeaving(true);
    await new Promise(resolve => window.setTimeout(resolve, 210));
    if (!mounted.current) return;
    close();
    setLeaving(false);
  }

  if (!open) return null;
  return <Modal title="Search Qobuz" className="catalog-search" motion={leaving ? "exit" : "enter"} dialogRef={dialog}
    cancel={() => void dismiss(true)} busy={search.writing || leaving} heading={id =>
      <header className="search-head">
        <div className="search-headline">
          <div>
            <h2 id={id}>Search Qobuz</h2>
            {!!search.snapshot?.history && <div className="search-history">
              <span>Artist activity · {search.snapshot.history} {search.snapshot.history === 1 ? "release" : "releases"}</span>
              <Button className="ghost" onClick={() => search.setResetting(true)} disabled={search.writing}>Clear search ranking history</Button>
            </div>}
            {search.snapshot?.historyError && <span className="search-status" role="status">Artist activity is temporarily unavailable.</span>}
          </div>
          <Button className="search-close" aria-label="Close search" onClick={() => void dismiss()} disabled={search.writing}><X /></Button>
        </div>
        <form className="search-query" onSubmit={event => { event.preventDefault(); search.queryChange(search.draft, true); }}>
          <Search className="icon" aria-hidden />
          <input ref={input} data-initial-focus type="search" aria-label="Search Qobuz catalog" placeholder="Search tracks, releases or artists"
            autoComplete="off" spellCheck={false} maxLength={512} value={search.draft}
            disabled={search.writing || leaving} onChange={event => search.queryChange(event.target.value)}
            onKeyDown={event => {
              if (event.key !== "Escape") return;
              event.preventDefault();
              event.stopPropagation();
              void dismiss(true);
            }} />
          <Button className="ghost" disabled={!search.draft || search.writing} onClick={() => {
            showGenres(false);
            search.queryChange(""); input.current?.focus();
          }}>Clear</Button>
        </form>
      </header>
    }>
    <div className="search-toolbar">
      <div className="search-tabs" role="tablist" aria-label="Search categories">
        {tabs.map(({ kind, label }, index) => <Button key={kind} role="tab" id={`search-tab-${kind}`} aria-controls="catalog-results"
          aria-selected={search.kind === kind} tabIndex={search.kind === kind ? 0 : -1}
          disabled={search.writing}
          onKeyDown={event => {
            const move = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
            if (!move) return;
            event.preventDefault();
            const next = tabs[(index + tabs.length + move) % tabs.length].kind;
            search.tabChange(next);
            document.getElementById(`search-tab-${next}`)?.focus();
          }}
          onClick={() => search.tabChange(kind)}>
          {label}{search.snapshot && <span className="search-tab-count">{search.snapshot.groups[kind].count}</span>}
        </Button>)}
      </div>
      <div className="search-controls">
        <Popover.Root open={filterOpen} onOpenChange={showGenres}>
          <Popover.Trigger className="search-field dropdown-trigger genre-trigger" ref={genreTrigger}
            aria-label="Genre filter" disabled={genreDisabled}>
            <span>{anyGenre ? "Any genre" : search.filter.length === 1 ? selectedGenre?.label : `${search.filter.length} genres`}</span>
            <ChevronDown className="icon" aria-hidden />
          </Popover.Trigger>
          <Popover.Portal container={dialog}>
            <Popover.Positioner className="dropdown-position genre-position" sideOffset={5} align="end"
              collisionPadding={8} collisionBoundary={dialog.current ?? undefined}>
              <Popover.Popup className="search-filter-menu" initialFocus={genreInput} finalFocus={genreTrigger} aria-label="Genres">
                <input ref={genreInput} type="search" aria-label="Search genres" placeholder="Search genres"
                  value={genreDraft} onChange={event => setGenreDraft(event.currentTarget.value)} />
                <div className="search-filter-head">
                  <Button onClick={() => search.filterChange(search.genres.map(genre => genre.id))}>Select all</Button>
                  <Button onClick={() => search.filterChange([])}>Deselect all</Button>
                </div>
                <GenreBadges open={filterOpen} genres={visibleGenres} labels={labels} selected={search.filter} change={search.filterChange} />
              </Popover.Popup>
            </Popover.Positioner>
          </Popover.Portal>
        </Popover.Root>
        <AnimatedSelect compact wide portal={dialog} label="Sort results (artist activity first)" items={sortOptions}
          value={search.sort} change={search.sortChange} disabled={search.writing} />
        <MovingChoices className="mode-picker search-view" role="group" aria-label="Result layout" value={search.layout}>
          <Button data-choice="cards" aria-label="Cards" title="Cards" aria-pressed={search.layout === "cards"} onClick={() => search.setLayout("cards")}><Grid2X2 className="icon" /></Button>
          <Button data-choice="table" aria-label="Table" title="Table" aria-pressed={search.layout === "table"} onClick={() => search.setLayout("table")}><List className="icon" /></Button>
        </MovingChoices>
      </div>
    </div>
    <div id="catalog-results" className="search-body" role="tabpanel" aria-labelledby={`search-tab-${search.kind}`} ref={scroll}>
      {search.frame && <div className="search-detail">
        <div className="search-detail-actions">
          <Button className="with-icon" onClick={search.back} disabled={search.writing}><ArrowLeft className="icon" /> Back</Button>
          {numbered && <Button aria-pressed={search.frame.whole} disabled={(!search.previewEligible && detail?.row.streamable !== true) || search.writing} onClick={search.toggleWhole}>Select release</Button>}
          {!numbered && search.eligible.length > 0 && <Button disabled={search.writing} onClick={() => {
            search.toggle(search.eligible.map(row => row.key), search.chosen.length !== search.eligible.length);
          }}>{search.chosen.length === search.eligible.length ? "Deselect loaded releases" : `Select ${search.eligible.length} loaded releases`}</Button>}
        </div>
        {detail && <div className="search-detail-summary">
          <Artwork key={searchKey(detail.row.key)} row={detail.row} generation={generation} />
          <div><span className="search-meta">{numbered ? "Release" : "Artist"}</span><h3>{detail.row.title}</h3>
            {detail.row.byline && <p>{detail.row.byline}</p>}
            {!numbered && <p>{detail.group.count} matching {detail.group.count === 1 ? "release" : "releases"} loaded</p>}
            <Quality row={detail.row} />
          </div>
        </div>}
      </div>}
      {search.failure && <p className="notice error" role="alert">{search.failure} <Button onClick={search.retry} disabled={search.pending || search.writing}>Retry</Button></p>}
      {!search.failure && account?.state !== "ready" && <div className="search-empty"><h3>{account?.state === "validating" ? "Validating Qobuz account…" : "Qobuz account required"}</h3><p>Validate your account in Settings → Account.</p></div>}
      {group?.error && <div className="notice error" role="alert">
        {group.count > 0 && "Partial results. "}{group.error.message}
        {group.error.retryable && <Button onClick={search.retry} disabled={search.pending || search.writing}>Retry</Button>}
      </div>}
      {group?.rows.length ? <>
        {layout === "table" && <div className="search-table-head">
          <span>{numbered ? "Track" : detail?.row.key.kind === "artist" ? "Releases" : tabs.find(tab => tab.kind === search.kind)?.label}</span>
          {group.rows[0].key.kind === "track" && <label className="search-bulk">
            <Checkbox label="Select visible tracks" checked={search.eligible.length > 0 && search.chosen.length === search.eligible.length && !search.frame?.whole}
              mixed={search.chosen.length > 0 && search.chosen.length < search.eligible.length && !search.frame?.whole}
              disabled={Boolean(search.frame?.whole) || !search.eligible.length || search.writing}
              change={event => search.toggle(search.eligible.map(row => row.key), event.target.checked)} />
            <span aria-hidden>Select</span>
          </label>}
        </div>}
        <ul className={`search-results ${layout}`}>
          {group.rows.map(row => <ResultRow key={searchKey(row.key)} row={row} generation={generation}
            numbered={numbered} genre={row.genreId === null ? undefined : labels.get(row.genreId)}
            previewOnly={search.previewEligible}
            disabled={(!search.previewEligible && row.streamable !== true) || search.writing || Boolean(search.frame?.whole)}
            checked={search.checked.has(searchKey(row.key))}
            toggle={on => search.toggle([row.key], on)}
            open={key => { if (!search.writing) void search.fetchDetail(key); }} />)}
        </ul>
        {search.pending && <p className="search-status" role="status">Loading more results…</p>}
      </> : search.pending ? <LoadingRows layout={layout} /> : !search.failure && !group?.error && account?.state === "ready" && (
        <div className="search-empty">
          <h3>{!search.draft.trim() ? "Search the Qobuz catalog" : group?.cursor || group?.limited ? "No matches in loaded results" : "No matches"}</h3>
          <p>{!search.draft.trim() ? "Enter a release, track, or artist name." : "Check the spelling, use at least three characters per prefix, or adjust the genre filters."}</p>
        </div>
      )}
      {group?.cursor && <Button className="search-more" disabled={search.pending || search.writing} onClick={loadMore}>Load more</Button>}
      {group?.limited && <p className="search-status">The loaded result limit was reached. Narrow the search to see other matches.</p>}
    </div>
    <footer className="search-footer">
      <span className="search-status" aria-live="polite">
        {search.chosen.length ? `${search.chosen.length} selected` : group ? `${group.count} loaded` : "Select items to continue"}
      </span>
      <div className="search-footer-actions">
        <Button className="with-icon" disabled={!canSubmit} onClick={() => void search.submit("bookmarks")}><Bookmark className="icon" /> Add to Bookmarks</Button>
        <Button className="primary with-icon" disabled={!canSubmit} onClick={() => void search.submit("downloads")}><Download className="icon" /> {account?.accessMode === "preview" ? "Queue MP3 previews" : "Add to Downloads"}</Button>
      </div>
    </footer>
    {(search.review || search.resetting) && <div className="search-confirm panel" role="alert">
      {search.review ? <>
        <strong>Add {search.review.result.count} selected releases?</strong>
        <p>Only the selected releases loaded for this artist will be added.</p>
        <div className="actions">
          <Button onClick={search.interrupt}>Cancel</Button>
          <Button className="primary" disabled={search.writing} onClick={() => {
            if (search.review?.result.token) void search.submit(search.review.destination, search.review.result.token);
          }}>Add {search.review.result.count} releases</Button>
        </div>
      </> : <>
        <strong>Clear search ranking history?</strong>
        <p>New activity will build your ranking history again.</p>
        <div className="actions"><Button onClick={() => search.setResetting(false)}>Cancel</Button>
          <Button className="danger" disabled={search.writing} onClick={() => void search.clearHistory()}>Clear history</Button>
        </div>
      </>}
    </div>}
    </Modal>;
}
import { Popover } from "@base-ui/react/popover";
