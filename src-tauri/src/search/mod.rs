pub mod activity;
mod rank;
mod submit;
#[cfg(test)]
mod tests;
pub use submit::{Review, Submission, SubmissionReport, commit, prepare};

use crate::{
    app::App,
    error::{AppError, Result},
    qobuz::{
        Credentials, QobuzError,
        search::{Id, Kind, Page, Row},
    },
};
use rank::{Ranked, Scores, Sort};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

const ROW_LIMIT: usize = 400;
const DETAIL_LIMIT: usize = 8;
const TIME_LIMIT: Duration = Duration::from_secs(8);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Taxonomy {
    storefront_coverage_verified: bool,
    storefront_nodes: usize,
    genres: Vec<Genre>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Genre {
    id: u32,
    label: String,
    parent: Option<u32>,
    path: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    storefront_path: Vec<u32>,
}

static GENRES: LazyLock<Vec<Genre>> = LazyLock::new(|| {
    let asset = serde_json::from_str::<Taxonomy>(include_str!("genres.json"))
        .expect("bundled genre taxonomy");
    assert!(asset.storefront_coverage_verified && asset.storefront_nodes == asset.genres.len());
    asset.genres
});

pub fn genres() -> &'static [Genre] {
    &GENRES
}

fn genre_matches(selected: &HashSet<u32>, id: Option<u32>) -> bool {
    selected.is_empty()
        || selected.len() == GENRES.len()
        || id.is_some_and(|id| {
            GENRES
                .iter()
                .find(|genre| genre.id == id)
                .is_some_and(|genre| {
                    genre
                        .path
                        .iter()
                        .chain(&genre.storefront_path)
                        .any(|ancestor| selected.contains(ancestor))
                })
        })
}

#[derive(Clone, Default)]
struct Candidates {
    rows: HashMap<String, Row>,
    offset: usize,
    total: Option<usize>,
    cursor: Option<String>,
    error: Option<AppError>,
    last_page: Vec<Option<String>>,
    limited: bool,
}

impl Candidates {
    fn push(&mut self, page: Page) {
        self.error = None;
        self.total = page.total.or(self.total);
        if page.length == 0 || page.ids == self.last_page || page.offset != self.offset {
            self.cursor = None;
            return;
        }
        self.last_page = page.ids;
        for row in page.rows {
            if self.rows.len() < ROW_LIMIT || self.rows.contains_key(&row.key.id) {
                self.rows.insert(row.key.id.clone(), row);
            } else {
                self.limited = true;
            }
        }
        self.offset = page.offset + page.length;
        let has_more = self.total.is_none_or(|total| self.offset < total);
        self.limited |= has_more && self.offset >= ROW_LIMIT;
        self.cursor = (has_more && !self.limited).then(new_token);
    }

    fn fail(&mut self, error: AppError) {
        self.error = Some(error);
    }
}

#[derive(Clone)]
struct Detail {
    row: Row,
    pages: Candidates,
}

#[derive(Clone)]
struct Catalog {
    generation: u64,
    query: String,
    words: Vec<String>,
    genres: HashSet<u32>,
    sort: Sort,
    groups: HashMap<Kind, Candidates>,
    relationships: HashMap<String, Arc<Row>>,
    details: HashMap<Id, Detail>,
    targeted: HashSet<Id>,
}

impl Catalog {
    fn new(
        generation: u64,
        query: String,
        words: Vec<String>,
        genres: HashSet<u32>,
        sort: Sort,
    ) -> Self {
        Self {
            generation,
            query,
            words,
            genres,
            sort,
            groups: Kind::ALL
                .into_iter()
                .map(|kind| (kind, Candidates::default()))
                .collect(),
            relationships: HashMap::new(),
            details: HashMap::new(),
            targeted: HashSet::new(),
        }
    }

    fn release_matches(&self, row: &Row) -> Option<rank::Match> {
        genre_matches(&self.genres, row.genre_id)
            .then(|| rank::matching(&self.words, row))
            .flatten()
    }

    fn releases(&self) -> HashMap<&str, &Row> {
        let mut releases = HashMap::new();
        for track in self.groups[&Kind::Track].rows.values() {
            if let Some(album) = &track.parent {
                releases
                    .entry(album.key.id.as_str())
                    .or_insert(album.as_ref());
            }
        }
        for (id, row) in &self.groups[&Kind::Album].rows {
            releases.insert(id, row);
        }
        for (id, row) in &self.relationships {
            releases.insert(id, row);
        }
        releases
    }

    fn artist_releases(&self, artist_id: &str) -> HashMap<&str, &Row> {
        let mut releases = self
            .releases()
            .into_iter()
            .filter(|(_, row)| row.credits.iter().any(|credit| credit.id == artist_id))
            .collect::<HashMap<_, _>>();
        let key = Id {
            kind: Kind::Artist,
            id: artist_id.to_owned(),
        };
        if let Some(detail) = self.details.get(&key) {
            for (id, album) in &detail.pages.rows {
                releases.insert(id, album);
            }
        }
        releases
    }

    fn ordered<'a>(&self, rows: impl IntoIterator<Item = &'a Row>, scores: &Scores) -> Vec<Row> {
        let mut rows = rows
            .into_iter()
            .filter_map(|row| {
                let genre_id = row
                    .parent
                    .as_ref()
                    .and_then(|album| album.genre_id)
                    .or(row.genre_id);
                if !genre_matches(&self.genres, genre_id) {
                    return None;
                }
                Some(Ranked::new(row, rank::matching(&self.words, row)?))
            })
            .collect::<Vec<_>>();
        rank::order(&mut rows, self.sort, scores);
        rows.into_iter().map(|entry| entry.row.clone()).collect()
    }

    fn rows(&self, kind: Kind, scores: &Scores) -> Vec<Row> {
        match kind {
            Kind::Album => self.ordered(self.releases().into_values(), scores),
            Kind::Track => {
                let albums = self.releases();
                let rows = self.groups[&kind]
                    .rows
                    .values()
                    .map(|row| {
                        let mut row = row.clone();
                        if let Some(album) = row.release.as_deref().and_then(|id| albums.get(id)) {
                            row.parent = Some(Arc::new((*album).clone()));
                            row.genre_id = album.genre_id.or(row.genre_id);
                        }
                        row
                    })
                    .collect::<Vec<_>>();
                self.ordered(&rows, scores)
            }
            Kind::Artist => {
                let mut artists = self.groups[&kind].rows.clone();
                let mut related: HashMap<&str, Vec<_>> = HashMap::new();
                for album in self.releases().into_values() {
                    if let Some(relevance) = self.release_matches(album) {
                        for credit in &album.credits {
                            let key = Id {
                                kind,
                                id: credit.id.clone(),
                            };
                            artists.entry(credit.id.clone()).or_insert_with(|| {
                                let mut artist = album.clone();
                                artist.key = key;
                                artist.title.clone_from(&credit.name);
                                artist.credits.clear();
                                artist.edition = None;
                                artist.artwork = None;
                                artist
                            });
                            related
                                .entry(&credit.id)
                                .or_default()
                                .push((relevance, album.date.as_deref()));
                        }
                    }
                }
                let mut rows = artists
                    .values()
                    .filter_map(|artist| {
                        let releases = related
                            .get(artist.key.id.as_str())
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        if !genre_matches(&self.genres, None) && releases.is_empty() {
                            return None;
                        }
                        let matches = releases
                            .iter()
                            .map(|(relevance, _)| *relevance)
                            .collect::<Vec<_>>();
                        let relevance = rank::artist_match(&self.words, artist, &matches)?;
                        Some(Ranked {
                            row: artist,
                            relevance,
                            date: releases.iter().filter_map(|(_, date)| *date).max(),
                        })
                    })
                    .collect::<Vec<_>>();
                rank::order(&mut rows, self.sort, scores);
                rows.into_iter()
                    .map(|item| {
                        let mut row = item.row.clone();
                        row.credits.clear();
                        row.byline = None;
                        row.edition = None;
                        row.genre_id = None;
                        row.hires = None;
                        row.explicit = None;
                        row.bit_depth = None;
                        row.sample_rate = None;
                        row.track_count = None;
                        row.date = item.date.map(str::to_owned);
                        row
                    })
                    .collect()
            }
        }
    }

    fn detail_rows(&self, key: &Id, scores: &Scores) -> Vec<Row> {
        if key.kind == Kind::Artist {
            return self.ordered(self.artist_releases(&key.id).into_values(), scores);
        }
        let Some(detail) = self.details.get(key) else {
            return Vec::new();
        };
        let mut rows = detail.pages.rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            (
                a.disc.unwrap_or(u32::MAX),
                a.number.unwrap_or(u32::MAX),
                &a.key.id,
            )
                .cmp(&(
                    b.disc.unwrap_or(u32::MAX),
                    b.number.unwrap_or(u32::MAX),
                    &b.key.id,
                ))
        });
        rows
    }

    fn page_source(&self, kind: Kind) -> Kind {
        if kind == Kind::Artist
            && self.groups[&kind].cursor.is_none()
            && self.groups[&kind].error.is_none()
        {
            return Kind::Album;
        }
        kind
    }

    fn snapshot(
        &self,
        scores: &Scores,
        history: usize,
        history_error: Option<AppError>,
    ) -> Snapshot {
        Snapshot {
            generation: self.generation,
            groups: Kind::ALL
                .into_iter()
                .map(|kind| {
                    let rows = self.rows(kind, scores);
                    let coverage = &self.groups[&self.page_source(kind)];
                    (
                        kind,
                        Group {
                            count: rows.len(),
                            rows,
                            cursor: coverage.cursor.clone(),
                            limited: coverage.limited,
                            error: coverage.error.clone(),
                            provider_total: self.groups[&kind].total,
                        },
                    )
                })
                .collect(),
            history,
            history_error,
        }
    }
}

#[derive(Default)]
pub struct Session {
    sequence: u64,
    cancel: CancellationToken,
    catalog: Option<Catalog>,
    review: Option<submit::Prepared>,
}

impl Session {
    pub fn invalidate(&mut self) {
        self.cancel.cancel();
        self.catalog = None;
        self.review = None;
    }

    pub fn cancel(&mut self, sequence: u64) {
        if sequence > self.sequence {
            self.sequence = sequence;
            self.cancel.cancel();
            self.review = None;
        }
    }

    fn start(&mut self, sequence: u64) -> Result<CancellationToken> {
        if sequence <= self.sequence {
            return Err(stale());
        }
        self.cancel(sequence);
        self.cancel = CancellationToken::new();
        Ok(self.cancel.clone())
    }

    fn check(&self, sequence: u64) -> Result<()> {
        if sequence != self.sequence || self.cancel.is_cancelled() {
            return Err(stale());
        }
        Ok(())
    }

    fn catalog(&self, generation: u64) -> Result<&Catalog> {
        self.catalog
            .as_ref()
            .filter(|catalog| catalog.generation == generation)
            .ok_or_else(stale)
    }
}

fn stale() -> AppError {
    AppError::new(
        "search_cancelled",
        "search",
        "This search request was superseded.",
        false,
    )
}

fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Filter,
    Sort,
    More,
    Retry,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    sequence: u64,
    generation: u64,
    query: String,
    genres: Vec<u32>,
    sort: Sort,
    kind: Kind,
    action: Action,
    cursor: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    rows: Vec<Row>,
    count: usize,
    cursor: Option<String>,
    provider_total: Option<usize>,
    limited: bool,
    error: Option<AppError>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    generation: u64,
    groups: HashMap<Kind, Group>,
    history: usize,
    history_error: Option<AppError>,
}

struct Budget {
    stop: CancellationToken,
    deadline: Instant,
}

impl Budget {
    fn new(stop: CancellationToken) -> Self {
        Self {
            stop,
            deadline: Instant::now() + TIME_LIMIT,
        }
    }

    async fn request<T>(
        &self,
        app: &App,
        account: &Credentials,
        work: impl Future<Output = std::result::Result<T, QobuzError>>,
    ) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(stale()),
            result = timeout_at(self.deadline, work) => match result {
                Ok(result) => result.map_err(public_error),
                Err(_) if app.qobuz.recovery_status(Some(account)).cooldown_seconds.is_some() => Err(rate_limited()),
                Err(_) => Err(AppError::new("search_timeout", "search", "Qobuz search took too long. Try again.", true)),
            },
        }
    }
}

fn rate_limited() -> AppError {
    AppError::new(
        "rate_limit",
        "search",
        "Qobuz is limiting requests. Wait a moment and retry.",
        true,
    )
}

fn public_error(error: QobuzError) -> AppError {
    let message = match error.code {
        "invalid_token" | "account_rejected" => {
            "Qobuz rejected this account. Check Account settings."
        }
        "rate_limit" => return rate_limited(),
        "network" | "api_timeout" => "Could not reach Qobuz. Check the connection and retry.",
        "resource_unavailable" => "This catalog item is unavailable for the current account.",
        _ => "Qobuz could not return these catalog results. Try again.",
    };
    AppError::new(error.code, "search", message, error.retryable)
}

fn account(state: &crate::app::AppState) -> Result<Credentials> {
    state
        .account
        .clone()
        .ok_or_else(|| {
            AppError::new(
                "search_account",
                "search",
                "Validate a Qobuz account in Settings to search the catalog.",
                false,
            )
        })
}

fn history(app: &App) -> (Scores, usize, Option<AppError>) {
    match app.storage.search_scores() {
        Ok((scores, count)) => (scores, count, None),
        Err(error) => (Scores::new(), 0, Some(error)),
    }
}

pub async fn query(app: &App, request: Query) -> Result<Snapshot> {
    if request.query.len() > 2048 || request.genres.len() > GENRES.len() {
        return Err(AppError::invalid(
            "search",
            "The search query or filter is too large.",
        ));
    }
    let genres: HashSet<_> = request.genres.into_iter().collect();
    if genres
        .iter()
        .any(|id| !GENRES.iter().any(|genre| genre.id == *id))
    {
        return Err(AppError::invalid(
            "search",
            "An unknown genre was selected.",
        ));
    }
    let words = rank::words(&request.query);
    let (mut catalog, stop, account) = {
        let mut state = app.state();
        state.ensure_open()?;
        let stop = state.search.start(request.sequence)?;
        let account = if words.is_empty() {
            None
        } else {
            Some(account(&state)?)
        };
        let catalog = match request.action {
            Action::Start => Catalog::new(
                request.generation,
                request.query,
                words,
                genres,
                request.sort,
            ),
            Action::Filter | Action::Sort | Action::More | Action::Retry => {
                let previous = state
                    .search
                    .catalog
                    .as_ref()
                    .filter(|catalog| catalog.query == request.query)
                    .ok_or_else(stale)?;
                if !matches!(request.action, Action::Filter)
                    && previous.generation != request.generation
                {
                    return Err(stale());
                }
                let mut catalog = previous.clone();
                catalog.generation = request.generation;
                catalog.sort = request.sort;
                if matches!(request.action, Action::Filter) {
                    catalog.genres = genres;
                    catalog.details.clear();
                }
                catalog
            }
        };
        (catalog, stop, account)
    };
    let budget = Budget::new(stop);
    if let Some(account) = &account {
        if matches!(request.action, Action::Start) {
            match budget
                .request(
                    app,
                    account,
                    app.qobuz.search_catalog(&catalog.query, account),
                )
                .await
            {
                Ok(groups) => {
                    for (kind, page) in groups {
                        let candidates = catalog.groups.get_mut(&kind).unwrap();
                        match page {
                            Ok(page) => candidates.push(page),
                            Err(error) => candidates.fail(public_error(error)),
                        }
                    }
                }
                Err(error) => {
                    for candidates in catalog.groups.values_mut() {
                        candidates.fail(error.clone());
                    }
                }
            }
        }
        let source = catalog.page_source(request.kind);
        let group = &catalog.groups[&source];
        let offset = group.offset;
        let refill = match request.action {
            Action::More => {
                if group.cursor.is_none() || group.cursor != request.cursor {
                    return Err(stale());
                }
                true
            }
            Action::Retry => group.error.is_some(),
            Action::Start | Action::Filter => {
                group.cursor.is_some() && catalog.rows(request.kind, &Scores::new()).len() < 5
            }
            Action::Sort => false,
        };
        if refill {
            let page = budget
                .request(
                    app,
                    account,
                    app.qobuz
                        .search_page(&catalog.query, source, offset, account),
                )
                .await;
            let group = catalog.groups.get_mut(&source).unwrap();
            match page {
                Ok(page) => group.push(page),
                Err(error) => group.fail(error),
            }
        }
        if !matches!(request.action, Action::Sort) && catalog.targeted.len() < ROW_LIMIT {
            enrich(app, account, &budget, &mut catalog, request.kind).await;
        }
    }
    let (scores, history_count, history_error) = history(app);
    let result = catalog.snapshot(&scores, history_count, history_error);
    let mut state = app.state();
    state.search.check(request.sequence)?;
    state.search.catalog = Some(catalog);
    Ok(result)
}

async fn enrich(
    app: &App,
    account: &Credentials,
    budget: &Budget,
    catalog: &mut Catalog,
    kind: Kind,
) {
    let missing_parent = catalog.groups[&Kind::Track]
        .rows
        .values()
        .filter_map(|track| {
            let release = track.release.as_ref()?;
            let key = Id {
                kind: Kind::Album,
                id: release.clone(),
            };
            let needs_parent = track
                .parent
                .as_ref()
                .is_none_or(|parent| parent.credits.is_empty() || parent.genre_id.is_none());
            (needs_parent
                && !catalog.relationships.contains_key(release)
                && !catalog.targeted.contains(&key))
            .then_some(key)
        })
        .min_by(|a, b| a.id.cmp(&b.id));
    let key = missing_parent.or_else(|| {
        if kind != Kind::Track || catalog.rows(kind, &Scores::new()).len() >= 5 {
            return None;
        }
        catalog
            .rows(Kind::Album, &Scores::new())
            .into_iter()
            .find(|album| !catalog.targeted.contains(&album.key))
            .map(|album| album.key)
    });
    if let Some(key) = key {
        match budget
            .request(app, account, app.qobuz.search_detail(&key, 0, account))
            .await
        {
            Ok((album, tracks)) => {
                catalog
                    .relationships
                    .insert(key.id.clone(), Arc::new(album));
                catalog.targeted.insert(key);
                if kind == Kind::Track
                    && let Some(tracks) = tracks
                {
                    let candidates = catalog.groups.get_mut(&Kind::Track).unwrap();
                    for row in tracks.rows {
                        if candidates.rows.len() < ROW_LIMIT
                            || candidates.rows.contains_key(&row.key.id)
                        {
                            candidates.rows.insert(row.key.id.clone(), row);
                        }
                    }
                }
            }
            Err(error) => {
                let group = catalog.groups.get_mut(&kind).unwrap();
                if group.error.is_none() {
                    group.fail(error);
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailRequest {
    sequence: u64,
    generation: u64,
    key: Id,
    cursor: Option<String>,
    retry: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailResult {
    row: Row,
    group: Group,
}

pub async fn detail(app: &App, request: DetailRequest) -> Result<DetailResult> {
    if !request.key.valid() || request.key.kind == Kind::Track {
        return Err(AppError::invalid(
            "search",
            "Choose a release or artist to open.",
        ));
    }
    let (mut catalog, stop, account) = {
        let mut state = app.state();
        state.ensure_open()?;
        let stop = state.search.start(request.sequence)?;
        (
            state.search.catalog(request.generation)?.clone(),
            stop,
            account(&state)?,
        )
    };
    let key = request.key;
    let initial = catalog
        .rows(key.kind, &Scores::new())
        .into_iter()
        .find(|row| row.key == key)
        .or_else(|| {
            catalog
                .details
                .keys()
                .filter(|key| key.kind == Kind::Artist)
                .find_map(|artist| {
                    catalog
                        .detail_rows(artist, &Scores::new())
                        .into_iter()
                        .find(|row| row.key == key)
                })
        })
        .or_else(|| {
            catalog
                .rows(Kind::Track, &Scores::new())
                .into_iter()
                .find_map(|track| {
                    track
                        .parent
                        .filter(|row| row.key == key)
                        .map(|row| (*row).clone())
                })
        });
    let mut detail = catalog.details.remove(&key).or_else(|| {
        initial.map(|row| Detail {
            row,
            pages: Candidates::default(),
        })
    });
    let linked = key.kind == Kind::Album
        && catalog
            .rows(Kind::Track, &Scores::new())
            .iter()
            .any(|track| track.release.as_deref() == Some(key.id.as_str()));
    if detail.is_none() && !linked {
        return Err(stale());
    }
    if request.cursor.is_some()
        && request.cursor.as_ref()
            != detail
                .as_ref()
                .and_then(|detail| detail.pages.cursor.as_ref())
    {
        return Err(stale());
    }
    if request.cursor.is_some()
        || request.retry
        || detail
            .as_ref()
            .is_none_or(|detail| detail.pages.total.is_none() && detail.pages.offset == 0)
    {
        let budget = Budget::new(stop);
        let offset = detail.as_ref().map_or(0, |detail| detail.pages.offset);
        match budget
            .request(
                app,
                &account,
                app.qobuz.search_detail(&key, offset, &account),
            )
            .await
        {
            Ok((row, page)) => {
                let pages = detail
                    .take()
                    .map_or_else(Candidates::default, |detail| detail.pages);
                let detail = detail.insert(Detail { row, pages });
                if let Some(page) = page {
                    detail.pages.push(page);
                }
            }
            Err(error) => {
                if let Some(detail) = &mut detail {
                    detail.pages.fail(error);
                } else {
                    return Err(error);
                }
            }
        }
    }
    let detail = detail.ok_or_else(stale)?;
    if catalog.details.len() >= DETAIL_LIMIT
        && let Some(remove) = catalog
            .details
            .keys()
            .find(|id| id.kind == Kind::Album)
            .or_else(|| catalog.details.keys().next())
            .cloned()
    {
        catalog.details.remove(&remove);
    }
    catalog.details.insert(key.clone(), detail.clone());
    let (scores, _, _) = history(app);
    let rows = catalog.detail_rows(&key, &scores);
    let group = Group {
        count: rows.len(),
        rows,
        cursor: detail.pages.cursor,
        limited: detail.pages.limited,
        error: detail.pages.error,
        provider_total: detail.pages.total,
    };
    let mut state = app.state();
    state.search.check(request.sequence)?;
    state.search.catalog = Some(catalog);
    Ok(DetailResult {
        row: detail.row,
        group,
    })
}

pub async fn artwork(app: &App, generation: u64, key: Id) -> Result<Option<String>> {
    let (url, cancel) = {
        let state = app.state();
        let catalog = state.search.catalog(generation)?;
        let url = catalog
            .details
            .get(&key)
            .and_then(|detail| detail.row.artwork.clone())
            .or_else(|| {
                catalog.groups[&key.kind]
                    .rows
                    .get(&key.id)
                    .and_then(|row| row.artwork.clone())
            })
            .or_else(|| {
                catalog.details.values().find_map(|detail| {
                    detail
                        .pages
                        .rows
                        .get(&key.id)
                        .filter(|row| row.key == key)
                        .and_then(|row| row.artwork.clone())
                })
            })
            .or_else(|| {
                (key.kind == Kind::Album)
                    .then(|| catalog.releases().get(key.id.as_str())?.artwork.clone())
                    .flatten()
            });
        (url, state.account_cancel.clone())
    };
    let Some(url) = url else {
        return Ok(None);
    };
    let image = tokio::select! {
        _ = cancel.cancelled() => return Err(stale()),
        image = app.media.thumbnail(&url) => image,
    };
    app.state().search.catalog(generation)?;
    Ok(image)
}

pub fn clear_history(app: &App) -> Result<String> {
    let state = app.state();
    state.ensure_open()?;
    Ok(app.storage.reset_search_activity()?.to_string())
}
