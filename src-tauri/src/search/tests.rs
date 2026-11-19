use super::*;
use crate::{qobuz::search::row, vault::Vault};
use serde_json::json;

fn settings(query: &str, sequence: u64, kind: Kind) -> Query {
    Query {
        sequence,
        generation: sequence,
        query: query.into(),
        genres: Vec::new(),
        sort: Sort::Relevance,
        kind,
        action: Action::Start,
        cursor: None,
    }
}

#[test]
fn filtered_pages_advance_by_provider_length_and_stop_on_replay() {
    let mut candidates = Candidates::default();
    let page = |offset| Page {
        offset,
        length: 20,
        rows: Vec::new(),
        total: Some(100),
        ids: (0..20).map(|id| Some(id.to_string())).collect(),
    };
    candidates.push(page(0));
    assert_eq!(candidates.offset, 20);
    assert!(candidates.cursor.is_some());
    candidates.push(page(20));
    assert!(candidates.cursor.is_none());
}

#[test]
fn bundle_paths_reference_known_distinct_genres() {
    let ids = genres()
        .iter()
        .map(|genre| genre.id)
        .collect::<HashSet<_>>();
    assert_eq!(ids.len(), genres().len());
    assert_eq!(ids.len(), 183);
    for genre in genres() {
        assert_eq!(genre.path.first().copied(), Some(genre.id));
        assert_eq!(genre.path.get(1).copied(), genre.parent);
        assert_eq!(
            genre.path.len(),
            genre.path.iter().copied().collect::<HashSet<_>>().len()
        );
        assert!(genre.path.iter().all(|id| ids.contains(id)));
        assert!(genre.storefront_path.iter().all(|id| ids.contains(id)));
        assert!(!genre.label.trim().is_empty());
    }
    let child = genres()
        .iter()
        .find(|genre| genre.parent.is_some())
        .unwrap();
    assert!(genre_matches(
        &HashSet::from([child.parent.unwrap()]),
        Some(child.id)
    ));
    assert!(!genre_matches(
        &HashSet::from([child.parent.unwrap()]),
        None
    ));
    assert!(genre_matches(&ids, None));
    assert!(genre_matches(&HashSet::from([91]), Some(197)));
    assert!(genre_matches(&HashSet::from([112]), Some(119)));
}

#[test]
fn derivation_and_track_context_keep_their_scopes() {
    let mut catalog = Catalog::new(
        1,
        "petal ari".into(),
        rank::words("petal ari"),
        HashSet::new(),
        Sort::Relevance,
    );
    let album = json!({"id":"001","title":"Petal","streamable":true,"artist":{"id":10,"name":"Ariana Grande"}});
    let context = row(Kind::Track, &json!({"id":1,"title":"Window Light","album":album,"performer":{"id":10,"name":"Ariana Grande"}}), None).unwrap();
    let direct = row(
        Kind::Track,
        &json!({"id":2,"title":"Petal","album":album,"performer":{"id":10,"name":"Ariana Grande"}}),
        None,
    )
    .unwrap();
    let unrelated = row(Kind::Track, &json!({"id":3,"title":"Petal Ari","album":{"id":"other","title":"Another Album","artist":{"id":50,"name":"Other"}}}), None).unwrap();
    catalog.groups.get_mut(&Kind::Track).unwrap().rows.extend(
        [context, direct, unrelated]
            .into_iter()
            .map(|row| (row.key.id.clone(), row)),
    );
    assert_eq!(
        catalog
            .rows(Kind::Track, &Scores::new())
            .iter()
            .map(|row| row.key.id.as_str())
            .collect::<Vec<_>>(),
        ["3", "2", "1"]
    );
    assert_eq!(
        catalog
            .rows(Kind::Artist, &Scores::new())
            .iter()
            .map(|row| row.key.id.as_str())
            .collect::<Vec<_>>(),
        ["10"]
    );
}

#[tokio::test]
async fn cancellation_drops_the_active_future_and_rejects_a_late_generation() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::initialize_root(dir.path().to_path_buf()).unwrap();
    let (sent, received) = tokio::sync::oneshot::channel::<()>();
    let stop = app.state().search.start(1).unwrap();
    let budget = Budget::new(stop);
    let task = async {
        let _drop_signal = sent;
        std::future::pending::<std::result::Result<(), QobuzError>>().await
    };
    let account = Credentials::default();
    let result = tokio::join!(budget.request(&app, &account, task), async {
        tokio::task::yield_now().await;
        app.state().search.start(2).unwrap();
    });
    assert_eq!(result.0.unwrap_err().code, "search_cancelled");
    assert!(received.await.is_err());
    assert!(app.state().search.check(1).is_err());
    assert!(app.state().search.check(2).is_ok());
}

#[tokio::test]
async fn an_expired_search_budget_drops_an_unfinished_operation() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::initialize_root(dir.path().to_path_buf()).unwrap();
    let budget = Budget {
        stop: CancellationToken::new(),
        deadline: Instant::now(),
    };
    let work = std::future::pending::<std::result::Result<(), QobuzError>>();
    let result = budget.request(&app, &Credentials::default(), work).await;
    assert_eq!(result.unwrap_err().code, "search_timeout");
}

#[tokio::test]
async fn failed_bookmark_parent_insertion_leaves_the_child_and_ledger_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::initialize_root(dir.path().to_path_buf()).unwrap();
    let mut catalog = Catalog::new(
        1,
        "petal".into(),
        rank::words("petal"),
        HashSet::new(),
        Sort::Relevance,
    );
    let album = row(
        Kind::Album,
        &json!({
            "id":"001","title":"Petal","streamable":true,"artist":{"id":10,"name":"Ariana Grande"}
        }),
        None,
    )
    .unwrap();
    catalog
        .groups
        .get_mut(&Kind::Album)
        .unwrap()
        .rows
        .insert("001".into(), album);
    let mut child = crate::storage::Bookmark::new(
        1,
        crate::qobuz::normalize_link("https://play.qobuz.com/track/123").unwrap(),
        "Pending".into(),
    );
    child.release_id = Some("001".into());
    app.storage.save_bookmarks([&child]).unwrap();
    {
        let mut state = app.state();
        state.account = Some(Credentials::default());
        state.search.catalog = Some(catalog);
        state.next_bookmark = 2;
        state.bookmarks.insert(1, child);
    }
    app.storage
        .open()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER stop_parent BEFORE INSERT ON bookmarks
         BEGIN SELECT raise(ABORT, 'test insertion failure'); END;",
        )
        .unwrap();
    let selection = || {
        serde_json::from_value(json!({
            "generation":1,"category":"album","wholeRelease":false,
            "selected":[{"kind":"album","id":"001"}],"destination":"bookmarks"
        }))
        .unwrap()
    };
    let review = prepare(&app, selection()).unwrap();
    assert!(commit(&app, review.token.unwrap()).is_err());
    assert!(app.state().bookmarks.contains_key(&1));
    assert_eq!(app.storage.search_scores().unwrap().1, 0);
    app.storage
        .open()
        .unwrap()
        .execute_batch("DROP TRIGGER stop_parent;")
        .unwrap();
    let review = prepare(&app, selection()).unwrap();
    let report = commit(&app, review.token.unwrap()).unwrap();
    assert_eq!(report.accepted.len(), 1);
    assert!(!app.state().bookmarks.contains_key(&1));
    app.state().queue.clear_downloads();
    assert_eq!(app.storage.search_scores().unwrap().1, 1);
    app.shutdown().await;
}

#[tokio::test]
#[ignore = "Requires live Qobuz access and explicitly selected protected credentials"]
async fn live_search_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::initialize_root(dir.path().to_path_buf()).unwrap();
    let vault = Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
    let account = vault
        .credentials()
        .expect("Could not open the selected vault");
    #[cfg(not(target_os = "macos"))]
    let validation = app.qobuz.validate_account(account);
    #[cfg(target_os = "macos")]
    let validation = app.qobuz.validate_account(account, false, None);
    let (account, _) = tokio::time::timeout(Duration::from_secs(12), validation)
        .await
        .expect("Account validation timed out")
        .unwrap_or_else(|error| panic!("Account validation: {}", error.code));
    {
        let mut state = app.state();
        state.account = Some(account);
    }
    for (sequence, text) in [(1, "petal ariana"), (2, "petal ari")] {
        let start = Instant::now();
        let result = query(&app, settings(text, sequence, Kind::Album))
            .await
            .unwrap();
        let releases = &result.groups[&Kind::Album];
        let tracks = &result.groups[&Kind::Track];
        let artists = &result.groups[&Kind::Artist];
        let first_release = &releases.rows[0];
        assert!(
            first_release.title.eq_ignore_ascii_case("petal")
                && first_release
                    .credits
                    .iter()
                    .any(|credit| credit.id == "713899"),
            "{text} led with {:?}",
            first_release
        );
        let first_track = &tracks.rows[0];
        assert!(
            first_track.title.eq_ignore_ascii_case("petal")
                && first_track
                    .credits
                    .iter()
                    .any(|credit| credit.id == "713899"),
            "{text} led with {:?}",
            first_track
        );
        assert!(artists.rows.iter().any(|row| row.key.id == "713899"));
        assert!(
            releases
                .rows
                .iter()
                .any(|row| row.explicit == Some(true) && row.title.eq_ignore_ascii_case("petal"))
        );
        assert!(
            releases
                .rows
                .iter()
                .any(|row| row.explicit == Some(false) && row.title.eq_ignore_ascii_case("petal"))
        );
        println!(
            "{text}: {} releases, {} tracks, {} artists in {} ms",
            releases.count,
            tracks.count,
            artists.count,
            start.elapsed().as_millis()
        );
    }
    let mut next = settings("petal ari", 3, Kind::Track);
    next.generation = 2;
    next.action = Action::More;
    next.cursor = app.state().search.catalog(2).unwrap().groups[&Kind::Track]
        .cursor
        .clone();
    let next = query(&app, next).await.unwrap();
    assert!(next.groups[&Kind::Track].error.is_none());
    let artist = detail(
        &app,
        DetailRequest {
            sequence: 4,
            generation: 2,
            key: Id {
                kind: Kind::Artist,
                id: "713899".into(),
            },
            cursor: None,
            retry: false,
        },
    )
    .await
    .unwrap();
    assert!(artist.group.error.is_none());
    assert!(
        artist
            .group
            .rows
            .iter()
            .any(|row| row.key.id == "tyx5jpmnacu5o")
    );
    let key = Id {
        kind: Kind::Album,
        id: "tyx5jpmnacu5o".into(),
    };
    let album = detail(
        &app,
        DetailRequest {
            sequence: 5,
            generation: 2,
            key: key.clone(),
            cursor: None,
            retry: false,
        },
    )
    .await
    .unwrap();
    assert!(album.group.error.is_none(), "{:?}", album.group.error);
    assert_eq!(album.group.count, 12);
    assert!(
        album
            .group
            .rows
            .windows(2)
            .all(|window| (window[0].disc, window[0].number) <= (window[1].disc, window[1].number))
    );
    assert!(
        album
            .group
            .rows
            .iter()
            .any(|row| row.explicit == Some(false))
    );
    let prepared = prepare(&app, serde_json::from_value(json!({
        "generation":2,"category":"track","detail":key,"wholeRelease":true,"selected":[],"destination":"bookmarks"
    })).unwrap()).unwrap();
    assert_eq!(prepared.count, 1);
    let report = commit(&app, prepared.token.unwrap()).unwrap();
    assert_eq!(report.accepted, [key]);
    assert_eq!(
        app.storage
            .search_scores()
            .unwrap()
            .1,
        1
    );
    assert!(clear_history(&app).is_ok());
    assert_eq!(
        app.storage
            .search_scores()
            .unwrap()
            .1,
        0
    );
    assert_eq!(app.state().bookmarks.len(), 1);
    let result = query(&app, settings("beyonce", 6, Kind::Artist))
        .await
        .unwrap();
    assert_eq!(result.groups[&Kind::Artist].rows[0].title, "Beyoncé");
    let mut filtered = settings("beyonce", 7, Kind::Album);
    filtered.action = Action::Filter;
    filtered.genres = vec![80];
    let filtered = query(&app, filtered).await.unwrap();
    let releases = &filtered.groups[&Kind::Album];
    assert!(releases.error.is_none());
    assert!(
        releases
            .rows
            .iter()
            .all(|row| genre_matches(&HashSet::from([80]), row.genre_id))
    );
    assert!(releases.cursor.is_some());
    println!(
        "beyonce Jazz: {} eligible releases, more provider candidates={}",
        releases.count,
        releases.cursor.is_some()
    );
    for name in ["suonelo.sqlite3", "suonelo.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(dir.path().join("data").join(name)) {
            assert!(!bytes.windows(12).any(|window| window == b"petal ariana"));
        }
    }
    app.shutdown().await;
}
