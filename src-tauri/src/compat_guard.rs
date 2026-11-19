//! Candidate policy seam. Not enabled in application startup until native scope acceptance.
use rusqlite::{Connection, params};
use std::time::{Duration, Instant};
pub const TARGET: &str = "suonelo.exe";
pub trait Adapter {
    fn call(&mut self, method: &str) -> Result<Option<u32>, String>;
    fn explicit(&self) -> bool;
    fn child_protected(&self) -> bool;
}
pub struct Audit(pub Connection);
impl Audit {
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        db.busy_timeout(Duration::ZERO).map_err(|e| e.to_string())?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS guard_events(event_id INTEGER PRIMARY KEY, operation_id TEXT NOT NULL, sequence INTEGER NOT NULL, timestamp_utc TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, trigger TEXT NOT NULL, target TEXT NOT NULL CHECK(target='suonelo.exe'), phase TEXT NOT NULL, old_value INTEGER, requested_value INTEGER, observed_value INTEGER, error_code TEXT, app_version TEXT NOT NULL, rtss_version TEXT NOT NULL, UNIQUE(operation_id,sequence)); CREATE TRIGGER IF NOT EXISTS guard_no_update BEFORE UPDATE ON guard_events BEGIN SELECT RAISE(ABORT,'append-only'); END; CREATE TRIGGER IF NOT EXISTS guard_no_delete BEFORE DELETE ON guard_events BEGIN SELECT RAISE(ABORT,'append-only'); END; CREATE INDEX IF NOT EXISTS guard_time ON guard_events(timestamp_utc); CREATE INDEX IF NOT EXISTS guard_operation ON guard_events(operation_id,sequence);").map_err(|e|e.to_string())?;
        Ok(Self(db))
    }
    pub fn append(
        &self,
        op: &str,
        phase: &str,
        old: Option<u32>,
        observed: Option<u32>,
        error: Option<&str>,
    ) -> Result<(), String> {
        // One durable statement per stage. No external mutation and SQL transaction are conflated.
        self.0.execute("INSERT INTO guard_events(operation_id,sequence,trigger,target,phase,old_value,requested_value,observed_value,error_code,app_version,rtss_version) SELECT ?1,COALESCE(MAX(sequence),0)+1,'harness',?2,?3,?4,0,?5,?6,?7,'fixture' FROM guard_events WHERE operation_id=?1",params![op,TARGET,phase,old,observed,error,env!("CARGO_PKG_VERSION")]).map_err(|e|e.to_string())?;
        Ok(())
    }
}
/// Caller owns the adapter exclusively for the full sequence. Native calls must be brokered.
pub fn check(
    adapter: &mut dyn Adapter,
    audit: &Audit,
    enabled: bool,
    op: &str,
    budget: Duration,
) -> String {
    if !enabled {
        return "disabled".into();
    }
    let start = Instant::now();
    let mut step = |method| {
        if start.elapsed() >= budget {
            Err("timeout".into())
        } else {
            adapter.call(method)
        }
    };
    let result = (|| -> Result<String, String> {
        if step("presence")? == Some(0) {
            return Ok("skipped".into());
        }
        let exists = step("exists")? == Some(1);
        step("load")?;
        let old = if exists { step("get")? } else { None };
        if exists && old == Some(0) {
            return Ok("unchanged".into());
        }
        audit
            .append(op, "intent", old, None, None)
            .map_err(|_| "intent-unavailable")?;
        audit
            .append(op, "attempted", old, None, None)
            .map_err(|_| "intent-unavailable")?;
        for method in ["set", "save", "readback", "update"] {
            match step(method) {
                Ok(value) => {
                    if method == "readback" {
                        if value != Some(0) {
                            let _ =
                                audit.append(op, "failed", old, value, Some("persistence-unknown"));
                            return Err("persistence-unknown".into());
                        }
                        audit
                            .append(op, "persistence_verified", old, value, None)
                            .map_err(|_| "audit-unavailable")?;
                    }
                }
                Err(error) => {
                    let _ = audit.append(op, "failed", old, None, Some(&error));
                    return Err(error);
                }
            }
        }
        audit
            .append(op, "effective_verified", old, Some(0), None)
            .map_err(|_| "audit-unavailable")?;
        Ok("verified-explicit".into())
    })();
    // A host-only zero never implies child protection.
    match result {
        Ok(outcome)
            if (outcome == "unchanged" || outcome == "verified-explicit")
                && !adapter.child_protected() =>
        {
            "blocked-scope".into()
        }
        Ok(outcome) if outcome == "verified-explicit" && !adapter.explicit() => {
            "persistence-unknown".into()
        }
        Ok(outcome) => outcome,
        Err(error) => error,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqlite_busy_and_existing_data_are_preserved() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("audit.sqlite3");
        let a = Audit::open(&path).unwrap();
        a.0.execute_batch("CREATE TABLE settings(value); INSERT INTO settings VALUES('preserved'); BEGIN EXCLUSIVE;").unwrap();
        let b = Connection::open(&path).unwrap();
        b.busy_timeout(Duration::ZERO).unwrap();
        assert!(b.execute("INSERT INTO guard_events(operation_id,sequence,trigger,target,phase,app_version,rtss_version)VALUES('x',1,'test','suonelo.exe','intent','x','x')",[]).is_err());
        a.0.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            a.0.query_row("SELECT value FROM settings", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
        drop(a);
        drop(b);
        assert!(Audit::open(&path).is_ok());
    }
    #[test]
    fn history_is_append_only_and_indexed() {
        let t = tempfile::tempdir().unwrap();
        let a = Audit::open(&t.path().join("audit.db")).unwrap();
        a.0.execute_batch("WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) INSERT INTO guard_events(operation_id,sequence,trigger,target,phase,app_version,rtss_version) SELECT 'historic-'||x,1,'test','suonelo.exe','intent','test','test' FROM n;").unwrap();
        let plan:String=a.0.query_row("EXPLAIN QUERY PLAN SELECT phase FROM guard_events WHERE operation_id='historic-9' ORDER BY sequence",[],|r|r.get(3)).unwrap();
        assert!(plan.contains("INDEX"));
        assert!(
            a.0.execute("UPDATE guard_events SET phase='effective_verified'", [])
                .is_err()
        );
        assert!(a.0.execute("DELETE FROM guard_events", []).is_err());
        assert!(a.append("new", "intent", Some(2), None, None).is_ok());
        assert!(
            a.0.execute(
                "INSERT INTO guard_events SELECT * FROM guard_events WHERE operation_id='new'",
                []
            )
            .is_err()
        );
    }
    #[test]
    fn intent_and_reconciliation_do_not_invent_success() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("audit.db");
        {
            let a = Audit::open(&p).unwrap();
            a.append("crashed", "intent", Some(2), None, None).unwrap();
            a.append("crashed", "attempted", Some(2), None, None)
                .unwrap();
        }
        let a = Audit::open(&p).unwrap();
        a.append(
            "crashed",
            "reconciliation",
            Some(2),
            Some(0),
            Some("external-state-observed-not-attributed"),
        )
        .unwrap();
        assert_eq!(
            a.0.query_row(
                "SELECT count(*) FROM guard_events WHERE phase='effective_verified'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
