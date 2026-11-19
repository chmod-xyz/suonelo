use super::{Scores, new_token, stale};
use crate::{
    app::{App, Destination},
    error::{AppError, Result},
    qobuz::{
        CanonicalResource, normalize_link,
        search::{Id, Kind, Row},
    },
    storage::Bookmark,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Submission {
    generation: u64,
    category: Kind,
    detail: Option<Id>,
    whole_release: bool,
    selected: Vec<Id>,
    destination: Destination,
}

#[derive(Serialize)]
pub struct Review {
    pub(super) token: Option<String>,
    pub(super) count: usize,
    skipped: Vec<Id>,
}

pub(super) struct Prepared {
    token: String,
    revision: u64,
    destination: Destination,
    rows: Vec<Row>,
    track_parents: HashMap<String, String>,
    skipped: Vec<Id>,
}

#[derive(Default, Serialize)]
pub struct SubmissionReport {
    pub accepted: Vec<Id>,
    pub skipped: Vec<Id>,
    pub conflicts: Vec<Id>,
}

pub fn prepare(app: &App, request: Submission) -> Result<Review> {
    if request.selected.len() > super::ROW_LIMIT || request.selected.iter().any(|id| !id.valid()) {
        return Err(AppError::invalid("search", "Invalid catalog selection."));
    }
    let mut state = app.state();
    state.ensure_open()?;
    let can_preview = super::account(&state)?
        .access_mode
        .allows_preview_selection();
    let catalog = state.search.catalog(request.generation)?;
    let scores = Scores::new();
    let eligible = match &request.detail {
        Some(key) if request.whole_release => {
            let detail = catalog
                .details
                .get(key)
                .filter(|detail| detail.row.key.kind == Kind::Album)
                .ok_or_else(stale)?;
            vec![detail.row.clone()]
        }
        Some(key) => {
            if !catalog.details.contains_key(key) {
                return Err(stale());
            }
            catalog.detail_rows(key, &scores)
        }
        None if !request.whole_release => catalog.rows(request.category, &scores),
        _ => return Err(stale()),
    };
    let eligible = eligible
        .into_iter()
        .map(|row| (row.key.clone(), row))
        .collect::<HashMap<_, _>>();
    let mut selected = request.selected.into_iter().collect::<HashSet<_>>();
    if request.whole_release {
        selected = request.detail.into_iter().collect();
    }
    let mut skipped = Vec::new();
    let mut rows = Vec::new();
    for key in selected {
        let Some(row) = eligible.get(&key) else {
            return Err(stale());
        };
        if row.key.kind == Kind::Artist || (!can_preview && row.streamable != Some(true)) {
            skipped.push(key);
        } else {
            rows.push(row.clone());
        }
    }
    rows.sort_by(|a, b| {
        a.key
            .kind
            .name()
            .cmp(b.key.kind.name())
            .then(a.key.id.cmp(&b.key.id))
    });
    let track_parents = catalog.groups[&Kind::Track]
        .rows
        .values()
        .chain(
            catalog
                .details
                .values()
                .flat_map(|detail| detail.pages.rows.values()),
        )
        .filter_map(|row| Some((row.key.id.clone(), row.release.clone()?)))
        .collect();
    let count = rows.len();
    let token = (count > 0).then(new_token);
    state.search.review = token.as_ref().map(|token| Prepared {
        token: token.clone(),
        revision: state.settings.revision,
        destination: request.destination,
        rows,
        track_parents,
        skipped: skipped.clone(),
    });
    Ok(Review {
        token,
        count,
        skipped,
    })
}

fn canonical(row: &Row) -> Result<CanonicalResource> {
    let url = format!(
        "https://play.qobuz.com/{}/{}",
        row.key.kind.name(),
        row.key.id
    );
    normalize_link(&url).map_err(|e| AppError::qobuz(e, "search"))
}

pub fn commit(app: &Arc<App>, token: String) -> Result<SubmissionReport> {
    let mut state = app.state();
    state.ensure_open()?;
    super::account(&state)?;
    let prepared = state
        .search
        .review
        .take()
        .filter(|review| review.token == token && review.revision == state.settings.revision)
        .ok_or_else(stale)?;
    let mut result = SubmissionReport {
        skipped: prepared.skipped,
        ..SubmissionReport::default()
    };
    let reason = state.waiting_reason();
    match prepared.destination {
        Destination::Bookmarks => {
            let mut next_id = state.next_bookmark;
            let existing = state
                .bookmarks
                .values()
                .map(|bookmark| &bookmark.key)
                .collect::<HashSet<_>>();
            let mut additions = Vec::new();
            let mut covered = HashSet::new();
            let mut credits = Vec::new();
            for row in &prepared.rows {
                let link = canonical(row)?;
                if row.key.kind == Kind::Album {
                    covered.insert(row.key.id.as_str());
                }
                if existing.contains(&link.key) {
                    result.skipped.push(row.key.clone());
                    continue;
                }
                let mut bookmark = Bookmark::new(next_id, link, reason.to_owned());
                next_id += 1;
                bookmark.release_id.clone_from(&row.release);
                credits.extend(super::activity::Credit::from_row(row));
                additions.push(bookmark);
                result.accepted.push(row.key.clone());
            }
            let removals = state
                .bookmarks
                .iter()
                .filter_map(|(id, bookmark)| {
                    if bookmark.key.kind != crate::qobuz::ResourceKind::Track {
                        return None;
                    }
                    let release = bookmark
                        .release_id
                        .as_ref()
                        .or_else(|| prepared.track_parents.get(&bookmark.key.id))?;
                    covered.contains(release.as_str()).then_some(*id)
                })
                .collect::<Vec<_>>();
            app.storage.save_search_bookmarks(
                &additions,
                &removals,
                &credits,
            )?;
            state.next_bookmark = next_id;
            for id in removals {
                state.bookmarks.remove(&id);
            }
            for bookmark in additions {
                let id = bookmark.dto.id;
                state.bookmarks.insert(id, bookmark);
                app.resolve_bookmark(&mut state, id);
            }
            state.bookmarks_changed();
        }
        Destination::Downloads => {
            let settings = state.settings.clone();
            let existing = state
                .queue
                .jobs
                .values()
                .map(|job| &job.link.key)
                .collect::<HashSet<_>>();
            let mut additions = Vec::new();
            let mut releases = HashSet::new();
            for row in &prepared.rows {
                let link = canonical(row)?;
                if existing.contains(&link.key) {
                    result.skipped.push(row.key.clone());
                    if row.key.kind == Kind::Album {
                        releases.insert(row.key.id.clone());
                    }
                    continue;
                }
                if row.key.kind == Kind::Album {
                    releases.insert(row.key.id.clone());
                }
                additions.push(link);
                result.accepted.push(row.key.clone());
            }
            state.queue.enqueue_search(
                additions,
                &settings,
                reason,
                releases,
                prepared.track_parents,
            );
        }
    }
    Ok(result)
}
