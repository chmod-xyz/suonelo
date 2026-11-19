use crate::{
    error::{AppError, Result},
    qobuz::{ResolvedTrack, ResourceKind, search::Row},
    storage::Storage,
};
use rusqlite::{Connection, TransactionBehavior, params};
use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::rank::Scores;

const LEGACY_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS search_activity_reset (
        account TEXT PRIMARY KEY,
        cutoff INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS search_activity (
        account TEXT NOT NULL,
        artist TEXT NOT NULL,
        release TEXT NOT NULL,
        weight INTEGER NOT NULL CHECK (weight IN (1, 2)),
        last_activity INTEGER NOT NULL,
        PRIMARY KEY (account, artist, release)
    );
";

pub fn migrate(db: &Connection, from: u32) -> rusqlite::Result<()> {
    db.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS profile_search_reset (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            cutoff INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS profile_search_activity (
            artist TEXT NOT NULL,
            release TEXT NOT NULL,
            weight INTEGER NOT NULL CHECK (weight IN (1, 2)),
            last_activity INTEGER NOT NULL,
            PRIMARY KEY (artist, release)
        );
    ",
    )?;
    if from <= 3 {
        // Earlier profiles may predate the ledger. Filter each scope against
        // its own cutoff before merging so a reset cannot affect another scope.
        db.execute_batch(LEGACY_SCHEMA)?;
        db.execute_batch(
            "
            INSERT INTO profile_search_activity(artist, release, weight, last_activity)
            SELECT a.artist, a.release, max(a.weight), max(a.last_activity)
            FROM search_activity AS a
            LEFT JOIN search_activity_reset AS r ON a.account = r.account
            WHERE a.last_activity > coalesce(r.cutoff, 0)
            GROUP BY a.artist, a.release
            ON CONFLICT(artist, release) DO UPDATE SET
                weight=max(weight, excluded.weight),
                last_activity=max(last_activity, excluded.last_activity);
            DROP TABLE search_activity;
            DROP TABLE search_activity_reset;
        ",
        )?;
    }
    Ok(())
}

pub fn load_clock(db: &Connection) -> rusqlite::Result<()> {
    let cutoff = db.query_row(
        "SELECT coalesce(max(cutoff), 0) FROM profile_search_reset",
        [],
        |row| row.get(0),
    )?;
    LAST_EVENT.fetch_max(cutoff, Ordering::Relaxed);
    Ok(())
}

static LAST_EVENT: AtomicI64 = AtomicI64::new(0);

/// UTC microseconds with a monotonic boundary for events in the same process.
pub fn event_time() -> i64 {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    let previous = LAST_EVENT
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |prior| {
            Some(clock.max(prior.saturating_add(1)))
        })
        .expect("clock update always succeeds");
    clock.max(previous.saturating_add(1))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Credit {
    pub artist: String,
    pub release: String,
}

impl Credit {
    pub fn from_row(row: &Row) -> Option<Self> {
        use crate::qobuz::search::Kind;
        let release = match row.key.kind {
            Kind::Album => row.key.id.clone(),
            Kind::Track => row.release.clone()?,
            Kind::Artist => return None,
        };
        Some(Self {
            artist: row.primary_credit.clone()?,
            release,
        })
    }

    pub fn from_track(track: &ResolvedTrack, job_kind: ResourceKind) -> Option<Self> {
        let performer = if job_kind == ResourceKind::Track {
            &track.track["performer"]["id"]
        } else {
            &track.album["artist"]["id"]
        };
        let artist = performer
            .as_u64()
            .or_else(|| performer.as_str()?.parse().ok())?;
        if artist == 0 || track.album_id.is_empty() {
            return None;
        }
        Some(Self {
            artist: artist.to_string(),
            release: track.album_id.clone(),
        })
    }
}

impl Storage {
    pub fn search_scores(&self) -> Result<(Scores, usize)> {
        let db = self.open()?;
        let mut stmt = db
            .prepare(
                "SELECT artist, sum(weight), max(last_activity), count(*) FROM profile_search_activity
             GROUP BY artist",
            )
            .map_err(AppError::database)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get(1)?, row.get(2)?),
                    row.get::<_, u32>(3)?,
                ))
            })
            .map_err(AppError::database)?;
        let mut scores = Scores::new();
        let mut count = 0;
        for entry in rows {
            let (artist, score, releases) = entry.map_err(AppError::database)?;
            scores.insert(artist, score);
            count += releases as usize;
        }
        Ok((scores, count))
    }

    pub fn record_search_activity<'a>(
        &self,
        credits: impl IntoIterator<Item = &'a Credit>,
        weight: u8,
        at: i64,
    ) -> Result<()> {
        let mut db = self.open()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::database)?;
        for credit in credits {
            record(&tx, credit, weight, at)?;
        }
        tx.commit().map_err(AppError::database)
    }

    pub fn reset_search_activity(&self) -> Result<i64> {
        let mut db = self.open()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(AppError::database)?;
        let cutoff: i64 = tx
            .query_row(
                "INSERT INTO profile_search_reset VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET cutoff=max(cutoff+1, excluded.cutoff)
             RETURNING cutoff",
                [event_time()],
                |row| row.get(0),
            )
            .map_err(AppError::database)?;
        tx.execute("DELETE FROM profile_search_activity", [])
            .map_err(AppError::database)?;
        tx.commit().map_err(AppError::database)?;
        LAST_EVENT.fetch_max(cutoff, Ordering::Relaxed);
        Ok(cutoff)
    }
}

pub fn record(db: &Connection, credit: &Credit, weight: u8, at: i64) -> Result<()> {
    db.execute(
        "INSERT INTO profile_search_activity(artist, release, weight, last_activity)
         SELECT ?1, ?2, ?3, ?4
         WHERE ?4 > coalesce((SELECT cutoff FROM profile_search_reset WHERE id=1), 0)
         ON CONFLICT(artist, release) DO UPDATE SET
             weight=max(weight, excluded.weight), last_activity=max(last_activity, excluded.last_activity)",
        params![credit.artist, credit.release, weight, at],
    ).map_err(AppError::database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_legacy_scopes_and_preserves_resets_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.sqlite");
        let (storage, ..) = Storage::initialize(path.clone()).unwrap();
        let db = storage.open().unwrap();
        db.execute_batch("DROP TABLE profile_search_activity; DROP TABLE profile_search_reset; PRAGMA user_version=3;").unwrap();
        db.execute_batch(LEGACY_SCHEMA).unwrap();
        db.execute_batch(
            "
            INSERT INTO search_activity_reset VALUES ('a', 50);
            INSERT INTO search_activity VALUES
                ('a', '10', '001', 2, 60),
                ('a', '99', '002', 2, 50),
                ('b', '10', '001', 1, 100),
                ('b', '20', '003', 1, 10);
        ",
        )
        .unwrap();
        drop(db);

        let (storage, ..) = Storage::initialize(path.clone()).unwrap();
        let (scores, count) = storage.search_scores().unwrap();
        assert_eq!(count, 2);
        assert_eq!(scores["10"], (2, 100));
        assert_eq!(scores["20"], (1, 10));
        let (storage, ..) = Storage::initialize(path.clone()).unwrap();
        assert_eq!(storage.search_scores().unwrap(), (scores, count));

        let cutoff = storage.reset_search_activity().unwrap();
        let (storage, ..) = Storage::initialize(path).unwrap();
        let credit = Credit {
            artist: "10".into(),
            release: "001".into(),
        };
        storage
            .record_search_activity([&credit], 2, cutoff)
            .unwrap();
        assert_eq!(storage.search_scores().unwrap().1, 0);
        std::thread::scope(|threads| {
            for weight in [1, 2, 1] {
                let (storage, credit) = (&storage, &credit);
                threads.spawn(move || {
                    storage
                        .record_search_activity([credit], weight, event_time())
                        .unwrap()
                });
            }
        });
        assert_eq!(storage.search_scores().unwrap().0["10"].0, 2);

        let (separate, ..) = Storage::initialize(dir.path().join("other.sqlite")).unwrap();
        assert_eq!(separate.search_scores().unwrap().1, 0);
    }
}
