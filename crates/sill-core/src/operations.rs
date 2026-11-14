//! Durable host mutation receipts. An interrupted external side effect is never replayed.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sill_proto::host::Operation;

pub const MAX_RECEIPTS: i64 = 65_536;
fn error(code: &'static str, message: &str) -> anyhow::Error {
    crate::sfp::DomainError {
        code,
        message: message.into(),
        details: None,
    }
    .into()
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn key(id: &str) -> Result<String> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(sill_proto::rpc::Error::invalid_params(
            "operation_id must be 1–128 bytes without control characters",
        )
        .into());
    }
    Ok(digest(id.as_bytes()))
}
pub struct Journal<'a>(&'a Connection);
impl<'a> Journal<'a> {
    pub fn new(conn: &'a Connection) -> Result<Self> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS sfp_operations (id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, method TEXT NOT NULL, state TEXT NOT NULL, agent_id TEXT, code TEXT);")?;
        Ok(Self(conn))
    }
    /// Run only once when the daemon adopts its store, before serving requests.
    pub fn recover(&self) -> Result<()> {
        self.0.execute(
            "UPDATE sfp_operations SET state='unknown' WHERE state='running'",
            [],
        )?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Operation> {
        self.lookup(id)?
            .map(|(_, op)| op)
            .ok_or_else(|| error("not_found", "operation not found"))
    }
    fn lookup(&self, id: &str) -> Result<Option<(String, Operation)>> {
        let row = self
            .0
            .query_row(
                "SELECT fingerprint,method,state,agent_id,code FROM sfp_operations WHERE id=?1",
                [key(id)?],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(fingerprint, method, state, agent_id, code)| {
            Ok((
                fingerprint,
                Operation {
                    operation_id: id.into(),
                    method,
                    state: serde_json::from_value(Value::String(state))?,
                    agent_id,
                    code,
                },
            ))
        })
        .transpose()
    }
    /// Lookup happens before environment preflight: a retry returns its receipt even if the
    /// installed registry or agent configuration has since changed.
    pub fn existing(&self, id: &str, method: &str, input: &Value) -> Result<Option<Operation>> {
        let fingerprint = digest(&serde_json::to_vec(&(method, input))?);
        let Some((previous, operation)) = self.lookup(id)? else {
            return Ok(None);
        };
        if fingerprint != previous {
            return Err(error(
                "id_conflict",
                "operation_id was already used for different input",
            ));
        }
        Ok(Some(operation))
    }
    /// Caller serializes this transaction with other acceptance operations.
    pub fn begin(&self, id: &str, method: &str, input: &Value) -> Result<(Operation, bool)> {
        let tx = self.0.unchecked_transaction()?;
        let journal = Journal(&tx);
        if let Some(operation) = journal.existing(id, method, input)? {
            return Ok((operation, false));
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM sfp_operations", [], |r| r.get(0))?;
        if count >= MAX_RECEIPTS {
            return Err(error("limit_exceeded", "operation receipt ledger is full"));
        }
        tx.execute(
            "INSERT INTO sfp_operations(id,fingerprint,method,state) VALUES(?1,?2,?3,'running')",
            params![
                key(id)?,
                digest(&serde_json::to_vec(&(method, input))?),
                method
            ],
        )?;
        tx.commit()?;
        Ok((self.get(id)?, true))
    }
    pub fn finish(&self, id: &str, agent_id: Option<&str>, code: Option<&str>) -> Result<()> {
        let state = if code.is_some() {
            "failed"
        } else {
            "completed"
        };
        let count = self.0.execute("UPDATE sfp_operations SET state=?2,agent_id=?3,code=?4 WHERE id=?1 AND state='running'", params![key(id)?,state,agent_id,code])?;
        if count != 1 {
            return Err(error("invalid_state", "operation is not running"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sill_proto::host::OperationState;
    #[test]
    fn receipts_deduplicate_conflict_and_recover_without_retrying() {
        let db = Connection::open_in_memory().unwrap();
        let j = Journal::new(&db).unwrap();
        let input = json!({"agent_id":"fixture"});
        assert!(j.begin("request", "login", &input).unwrap().1);
        assert!(!j.begin("request", "login", &input).unwrap().1);
        assert!(j.begin("request", "install", &input).is_err());
        j.recover().unwrap();
        assert_eq!(j.get("request").unwrap().state, OperationState::Unknown);
        assert!(!j.begin("request", "login", &input).unwrap().1);
        assert!(j.finish("request", Some("fixture"), None).is_err());
        j.begin("completed", "login", &input).unwrap();
        j.finish("completed", Some("fixture"), None).unwrap();
        j.begin("failed", "install", &input).unwrap();
        j.finish("failed", None, Some("operation_failed")).unwrap();
        j.recover().unwrap();
        assert_eq!(j.get("completed").unwrap().state, OperationState::Completed);
        assert_eq!(j.get("failed").unwrap().state, OperationState::Failed);
        assert_eq!(
            j.get("completed").unwrap().agent_id.as_deref(),
            Some("fixture")
        );
        assert!(j.get("").is_err());
    }
    #[test]
    fn receipt_limit_refuses_new_work_but_preserves_retry() {
        let db = Connection::open_in_memory().unwrap();
        let j = Journal::new(&db).unwrap();
        j.begin("retained", "login", &Value::Null).unwrap();
        db.execute_batch("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n < 65535) INSERT INTO sfp_operations(id,fingerprint,method,state) SELECT 'fixture-' || n,'x','login','completed' FROM numbers;").unwrap();
        assert!(j.begin("new", "login", &Value::Null).is_err());
        assert!(!j.begin("retained", "login", &Value::Null).unwrap().1);
    }
}
