import { DndContext, PointerSensor, pointerWithin, useDraggable, useDroppable, useSensor, useSensors } from "@dnd-kit/core";
import { open } from "@tauri-apps/plugin-dialog";
import { GripVertical, Tag } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ComponentProps, type KeyboardEvent, type RefObject } from "react";
import { desktop, errorMessage, type Settings, type SettingsPatch } from "@/lib/desktop";
import { Button, Field, SettingSwitch } from "./ui/primitives";
import type { Notify } from "./toast";

const badges = [
  "artist", "album artist", "track title", "track number", "year", "album name",
  "album id", "target quality", "bit depth", "sampling rate", "genre", "disc number",
];
type FieldName = "folderTemplate" | "filenameTemplate";

function MetadataBadge({ badge, insert }: { badge: string; insert: () => void }) {
  const { attributes, listeners, setNodeRef, transform, isDragging } = useDraggable({ id: badge });
  return (
    <Button
      ref={setNodeRef}
      className="relative touch-none"
      data-metadata-badge
      data-dragging={isDragging || undefined}
      style={transform ? { zIndex: 1, transform: `translate(${transform.x}px, ${transform.y}px)` } : undefined}
      onClick={insert}
      {...attributes}
      {...listeners}
    >
      <GripVertical className="icon" aria-hidden /> %{badge}%
    </Button>
  );
}

function TemplateInput({ field, nodeRef, ...props }: ComponentProps<"input"> & {
  field: FieldName;
  nodeRef: RefObject<HTMLInputElement | null>;
}) {
  const { setNodeRef, isOver } = useDroppable({ id: field });
  const attach = useCallback((node: HTMLInputElement | null) => {
    nodeRef.current = node;
    setNodeRef(node);
  }, [nodeRef, setNodeRef]);
  return <input {...props} ref={attach} className="template-drop-target" data-over={isOver || undefined} />;
}

export function OutputSettings({ settings, patch, notify }: {
  settings: Settings;
  patch: (patch: SettingsPatch) => void;
  notify: Notify;
}) {
  const [folder, setFolder] = useState(settings.folderTemplate);
  const [filename, setFilename] = useState(settings.filenameTemplate);
  const [preview, setPreview] = useState("");
  const [previewError, setPreviewError] = useState("");
  const [path, setPath] = useState(settings.outputPath);
  const lastField = useRef<FieldName>("folderTemplate");
  const folderInput = useRef<HTMLInputElement>(null);
  const filenameInput = useRef<HTMLInputElement>(null);
  const persistedPath = useRef(settings.outputPath);
  const sensors = useSensors(useSensor(PointerSensor, { activationConstraint: { distance: 6 } }));
  useEffect(() => {
    if (persistedPath.current !== settings.outputPath) {
      persistedPath.current = settings.outputPath;
      setPath(settings.outputPath);
    }
  }, [settings.outputPath]);

  useEffect(() => {
    let current = true;
    const timer = setTimeout(async () => {
      try {
        const result = await desktop.previewOutput(folder, filename, settings.quality, settings.safeMode);
        if (!current) return;
        setPreview(result.path);
        setPreviewError("");
      } catch (error) {
        if (current) setPreviewError(errorMessage(error));
      }
    }, 160);
    return () => { current = false; clearTimeout(timer); };
  }, [folder, filename, settings.quality, settings.outputPath, settings.revision, settings.safeMode]);

  function update(field: FieldName, value: string) {
    (field === "folderTemplate" ? setFolder : setFilename)(value);
    patch({
      folderTemplate: field === "folderTemplate" ? value : folder,
      filenameTemplate: field === "filenameTemplate" ? value : filename,
    });
  }

  function replaceSelection(field: FieldName, text: string) {
    const input = (field === "folderTemplate" ? folderInput : filenameInput).current;
    if (!input) return;
    const start = input.selectionStart ?? input.value.length;
    const end = input.selectionEnd ?? start;
    update(field, input.value.slice(0, start) + text + input.value.slice(end));
    requestAnimationFrame(() => {
      input.focus();
      input.setSelectionRange(start + text.length, start + text.length);
    });
  }

  function moveBadge(event: KeyboardEvent<HTMLInputElement>, field: FieldName) {
    if (!event.altKey || !["ArrowLeft", "ArrowRight"].includes(event.key)) return;
    const input = event.currentTarget;
    const start = input.selectionStart ?? 0;
    const end = input.selectionEnd ?? start;
    const tokens = [...input.value.matchAll(/\[[^\[\]]*%[^%]+%[^\[\]]*\]|%[^%]+%/g)];
    const index = tokens.findIndex(token => token.index <= start && token.index + token[0].length >= end);
    if (index < 0) return;
    const nextIndex = index + (event.key === "ArrowLeft" ? -1 : 1);
    if (nextIndex < 0 || nextIndex >= tokens.length) return;
    event.preventDefault();
    const first = tokens[Math.min(index, nextIndex)];
    const second = tokens[Math.max(index, nextIndex)];
    const between = input.value.slice(first.index + first[0].length, second.index);
    const value = input.value.slice(0, first.index) + second[0] + between + first[0] + input.value.slice(second.index + second[0].length);
    const newStart = nextIndex < index ? first.index : first.index + second[0].length + between.length;
    update(field, value);
    requestAnimationFrame(() => input.setSelectionRange(newStart, newStart + tokens[index][0].length));
  }

  async function browse() {
    try {
      const selected = await open({ directory: true, multiple: false, defaultPath: settings.outputMissing ? undefined : path });
      if (typeof selected === "string") {
        setPath(selected);
        patch({ outputPath: selected });
      }
    } catch (error) {
      notify(errorMessage(error), "error");
    }
  }

  function templateInput(field: FieldName, value: string) {
    return (
      <TemplateInput
        field={field}
        nodeRef={field === "folderTemplate" ? folderInput : filenameInput}
        value={value}
        onFocus={() => { lastField.current = field; }}
        onChange={event => update(field, event.currentTarget.value)}
        onKeyDown={event => moveBadge(event, field)}
      />
    );
  }

  return (
    <>
      <Field label="Destination folder">
        <input value={path} onChange={event => {
          setPath(event.target.value);
          patch({ outputPath: event.target.value });
        }} />
      </Field>
      <Button onClick={browse}>Browse…</Button>
      {settings.outputMissing && <p className="notice">Choose an available destination to continue queued jobs.</p>}
      <DndContext
        id="output-badges"
        sensors={sensors}
        collisionDetection={pointerWithin}
        accessibility={{ screenReaderInstructions: {
          draggable: "Press Enter or Space to insert this badge into the last focused template field.",
        } }}
        onDragEnd={({ active, over }) => {
          const field = over?.id;
          if ((field === "folderTemplate" || field === "filenameTemplate") &&
              typeof active.id === "string" && badges.includes(active.id)) {
            replaceSelection(field, `%${active.id}%`);
          }
        }}
      >
        <Field label="Folder structure">{templateInput("folderTemplate", folder)}</Field>
        <Field label="Filename">{templateInput("filenameTemplate", filename)}</Field>
        <div className="chipbox">
          <h3><Tag className="icon" aria-hidden /> Metadata badges</h3>
          {badges.map(badge => (
            <MetadataBadge key={badge} badge={badge} insert={() => replaceSelection(lastField.current, `%${badge}%`)} />
          ))}
        </div>
      </DndContext>
      <div className="field">
        <span>Live preview</span>
        <small>Use multiple optional groups, such as %album name% [(%year%)] [%genre%]. Spaces inside a group disappear with missing metadata; spaces outside remain. Repeated interior spaces are preserved. Double brackets keep visible brackets: [[%bit depth%-Bits]] becomes [24-Bits]. %album title% is an alias for %album name%. Select a token and use Alt+Left or Alt+Right to reorder it.</small>
        <output className={`preview${previewError ? " error" : ""}`} aria-live="polite">
          {previewError || preview || "Preparing a sample path…"}
        </output>
      </div>
      <SettingSwitch name="createPlaylist" label="Create playlist" checked={settings.createPlaylist}
        description="Save an .m3u playlist in Qobuz order when downloading a playlist link."
        change={createPlaylist => patch({ createPlaylist })} />
      <SettingSwitch name="smartDiscography" label="Smart discography" checked={settings.smartDiscography}
        description="For artist links, pick one version of each album, favoring the highest available audio quality."
        change={smartDiscography => patch({ smartDiscography })} />
      <SettingSwitch name="safeMode" label="Safe mode" checked={settings.safeMode}
        description="Strips illegal characters for maximum compatibility. Applies to filenames and folder names."
        change={safeMode => patch({ safeMode })} />
    </>
  );
}
