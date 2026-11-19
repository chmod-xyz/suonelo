import { isMac } from "@/lib/platform";
import { useEffect, useRef, useState } from "react";
import {
  desktop, errorMessage, type Account, type Destination, type SearchAction, type SearchDetail,
  type SearchGenre, type SearchId, type SearchKind, type SearchReview, type SearchSnapshot, type SearchSort,
  type SearchSubmissionReport,
} from "./desktop";

interface DetailFrame {
  key: SearchId;
  value?: SearchDetail;
  whole: boolean;
}

export const searchKey = ({ kind, id }: SearchId) => `${kind}:${id}`;

export function useCatalogSearch(open: boolean, account: Account | undefined, beforeQueue: () => Promise<void>,
  onSubmit: (report: SearchSubmissionReport, destination: Destination) => void) {
  const [draft, setDraft] = useState("");
  const [kind, setKind] = useState<SearchKind>("album");
  const [sort, setSort] = useState<SearchSort>("relevance");
  const [layout, setLayout] = useState<"cards" | "table">("cards");
  const [genres, setGenres] = useState<SearchGenre[]>([]);
  const [filter, setFilter] = useState<number[]>([]);
  const [snapshot, setSnapshot] = useState<SearchSnapshot>();
  const [stack, setStack] = useState<DetailFrame[]>([]);
  const [selected, setSelected] = useState<Record<string, Set<string>>>({});
  const [pending, setPending] = useState(false);
  const [writing, setWriting] = useState(false);
  const [failure, setFailure] = useState("");
  const [review, setReview] = useState<{ result: SearchReview; destination: Destination }>();
  const [resetting, setResetting] = useState(false);
  const sequence = useRef(0);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const settled = useRef<{ query: string; generation: number; sort: SearchSort }>(undefined);
  const desiredSort = useRef<SearchSort>("relevance");

  const frame = stack.at(-1);
  const group = frame ? frame.value?.group : snapshot?.groups[kind];
  const scope = frame ? `detail:${searchKey(frame.key)}` : kind;
  const checked = selected[scope] ?? new Set<string>();
  const previewEligible = isMac ? account?.state === "ready" && account.accessMode !== "full" : account?.accessMode === "preview";
  const eligible = (group?.rows ?? []).filter(row => row.key.kind !== "artist" && (previewEligible || row.streamable === true));
  const chosen = frame?.whole && frame.value && (previewEligible || frame.value.row.streamable === true)
    ? [frame.key] : eligible.filter(row => checked.has(searchKey(row.key))).map(row => row.key);

  function next() {
    sequence.current = Math.max(Date.now() * 1000, sequence.current + 1);
    return sequence.current;
  }

  function interrupt() {
    clearTimeout(timer.current);
    void desktop.cancelSearch(next()).catch(() => undefined);
    setPending(false);
    setReview(undefined);
    setResetting(false);
  }

  async function run(query: string, genreIds: number[], action: SearchAction, category = kind, cursor?: string | null) {
    const request = next();
    const generation = action === "start" || action === "filter" ? request : settled.current?.generation;
    if (generation === undefined) return;
    const requestedSort = desiredSort.current;
    setPending(Boolean(query.trim()));
    setFailure("");
    try {
      const result = await desktop.search({ sequence: request, generation, query, genres: genreIds, sort: requestedSort, kind: category, action, cursor });
      if (sequence.current !== request) return;
      settled.current = { query, generation: result.generation, sort: requestedSort };
      setSnapshot(result);
      if (requestedSort !== desiredSort.current) {
        void run(query, genreIds, "sort", category);
      } else if (action === "sort" && frame?.key.kind === "artist" && frame.value) {
        void fetchDetail(frame.key, "refresh");
      }
    } catch (error) {
      if (sequence.current === request) setFailure(errorMessage(error));
    } finally {
      if (sequence.current === request) setPending(false);
    }
  }

  function queryChange(query: string, immediately = false) {
    interrupt();
    setDraft(query);
    setStack([]);
    setFailure("");
    if (settled.current?.query !== query) {
      settled.current = undefined;
      setSnapshot(undefined);
      setSelected({});
    }
    if (!query.trim() || immediately) {
      void run(query, filter, "start");
    } else {
      setPending(account?.state === "ready");
      timer.current = setTimeout(() => void run(query, filter, "start"), 250);
    }
  }

  function filterChange(ids: number[]) {
    const cached = settled.current?.query === draft;
    interrupt();
    setFilter(ids);
    setStack([]);
    setFailure("");
    setSnapshot(undefined);
    void run(draft, ids, cached ? "filter" : "start");
  }

  function sortChange(value: SearchSort) {
    desiredSort.current = value;
    setSort(value);
    setReview(undefined);
    if (!pending && settled.current?.query === draft) void run(draft, filter, "sort");
  }

  function tabChange(value: SearchKind) {
    if (frame || review) interrupt();
    setKind(value);
    setReview(undefined);
    setFailure("");
    setStack([]);
  }

  async function fetchDetail(key: SearchId, mode: "open" | "more" | "retry" | "refresh" = "open") {
    if (!snapshot) return;
    interrupt();
    const request = next();
    setPending(true);
    setFailure("");
    if (mode === "open") setStack(previous => [...previous, { key, whole: false }]);
    try {
      const value = await desktop.searchDetail({
        sequence: request, generation: snapshot.generation, key,
        cursor: mode === "more" ? frame?.value?.group.cursor : undefined, retry: mode === "retry",
      });
      if (sequence.current === request) {
        setStack(previous => previous.map((item, index) => index === previous.length - 1 ? { ...item, value } : item));
      }
    } catch (error) {
      if (sequence.current === request) setFailure(errorMessage(error));
    } finally {
      if (sequence.current === request) setPending(false);
    }
  }

  function back() {
    interrupt();
    setFailure("");
    setStack(previous => previous.slice(0, -1));
  }

  function toggleWhole() {
    if (review) interrupt();
    setStack(previous => previous.map((item, index) => index === previous.length - 1 ? { ...item, whole: !item.whole } : item));
  }

  function toggle(keys: SearchId[], on: boolean) {
    if (review) interrupt();
    setSelected(previous => {
      const items = new Set(previous[scope]);
      for (const key of keys) {
        if (on) items.add(searchKey(key));
        else items.delete(searchKey(key));
      }
      return { ...previous, [scope]: items };
    });
  }

  async function submit(destination: Destination, token?: string) {
    if (writing || pending || !snapshot || !chosen.length) return;
    const request = sequence.current;
    setWriting(true);
    setFailure("");
    try {
      if (destination === "downloads" && !token) await beforeQueue();
      if (sequence.current !== request) return;
      if (!token) {
        const result = await desktop.prepareSearch({
          generation: snapshot.generation, category: kind, detail: frame?.key,
          wholeRelease: Boolean(frame?.whole), selected: chosen, destination,
        });
        if (sequence.current !== request) return;
        if (!result.token) {
          setFailure("None of these items are available to submit for this account.");
          return;
        }
        if (frame?.key.kind === "artist") {
          setReview({ result, destination });
          return;
        }
        token = result.token;
      }
      const report = await desktop.commitSearch(token);
      if (sequence.current !== request) return;
      setReview(undefined);
      toggle(report.accepted, false);
      if (frame?.whole && report.accepted.length) toggleWhole();
      onSubmit(report, destination);
      if (!frame) void run(draft, filter, "sort");
    } catch (error) {
      setReview(undefined);
      if (sequence.current === request) setFailure(errorMessage(error));
    } finally {
      setWriting(false);
    }
  }

  async function clearHistory() {
    const request = sequence.current;
    setWriting(true);
    setResetting(false);
    try {
      await desktop.clearSearchHistory();
      if (sequence.current !== request) return;
      setStack([]);
      void run(draft, filter, settled.current?.query === draft ? "sort" : "start");
    } catch (error) {
      if (sequence.current === request) setFailure(errorMessage(error));
    } finally {
      setWriting(false);
    }
  }

  useEffect(() => {
    if (!open) {
      interrupt();
      setStack(previous => previous.filter(item => item.value));
      return;
    }
    let mounted = true;
    void desktop.searchGenres().then(value => { if (mounted) setGenres(value); }).catch(() => undefined);
    if (account?.state === "ready") {
      void run(draft, filter, settled.current?.query === draft ? "sort" : "start");
    }
    return () => { mounted = false; clearTimeout(timer.current); };
  }, [open, account?.state]);

  return {
    draft, kind, sort, layout, setLayout, genres, filter, snapshot, frame, group, pending, writing, failure, checked, previewEligible, eligible, chosen,
    review, setReview, resetting, setResetting, clearHistory, submit, queryChange, filterChange, sortChange, tabChange, fetchDetail,
    back, toggleWhole, toggle, interrupt,
    more: () => frame ? void fetchDetail(frame.key, "more") : void run(draft, filter, "more", kind, group?.cursor),
    retry: () => frame ? void fetchDetail(frame.key, "retry") : void run(draft, filter, settled.current ? "retry" : "start"),
  };
}
