import { useState } from "react";
import type { MetadataField, Settings, SettingsPatch } from "@/lib/desktop";
import { Field } from "./ui/primitives";

export function AdvancedMetadata({ fields, settings, patch }: {
  fields: MetadataField[]; settings: Settings; patch: (changes: SettingsPatch) => void;
}) {
  const [search, setSearch] = useState("");
  const query = search.trim().toLocaleLowerCase();
  const matches = (field: MetadataField) => [field.label, field.id, field.exportTag].some(value => value?.toLocaleLowerCase().includes(query));
  const shown = fields.filter(matches);
  return (
    <details className="advanced-metadata">
      <summary>Advanced Qobuz metadata</summary>
      <p>Raw Qobuz fields are opt-in and independent of standard tags. The export identity is shown below each friendly label.</p>
      <Field label="Search Qobuz fields">
        <input type="search" value={search} onChange={event => setSearch(event.target.value)} placeholder="Search labels or raw paths" />
      </Field>
      {(["album", "track"] as const).map(group => {
        const grouped = shown.filter(field => (field.group || field.id.split("/")[0]) === group);
        return grouped.length > 0 && (
          <section key={group} aria-label={group === "album" ? "Album fields" : "Track fields"}>
            <h4>{group === "album" ? "Album" : "Track"}</h4>
            <div className="raw-grid">
              {grouped.map(field => (
                <label className="raw-field" key={field.id}>
                  <input type="checkbox" checked={settings.tags[field.id] ?? false}
                    onChange={event => patch({ tags: { ...settings.tags, [field.id]: event.target.checked } })} />
                  <span>
                    <span>{field.label}</span>
                    <code>{field.rawPath || field.id}</code>
                    <code>{field.exportTag || `QOBUZ:${field.id}`}</code>
                  </span>
                </label>
              ))}
            </div>
          </section>
        );
      })}
      {shown.length === 0 && <p>{fields.length ? "No matching Qobuz fields." : "Raw fields appear after Qobuz metadata is resolved."}</p>}
    </details>
  );
}
