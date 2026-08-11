use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use gobrowse_core::sandbox::{
    MAX_TERMINAL_OUTPUT_BYTES, MAX_TERMINAL_OUTPUT_CHUNK_BYTES, ResponseEnvelope, TerminalState,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;
use uuid::Uuid;

type StoredMutation = (String, String, Vec<u8>, String, Option<Vec<u8>>);

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("terminal journal is unavailable")]
    Sqlite(#[from] rusqlite::Error),
    #[error("terminal journal lock is poisoned")]
    Poisoned,
    #[error("terminal journal contains invalid data")]
    Corrupt,
    #[error("terminal identifier conflicts with an existing terminal")]
    Conflict,
    #[error("terminal does not exist")]
    NotFound,
    #[error("a previous operation may have taken effect")]
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRecord {
    pub terminal_id: Uuid,
    pub workspace_id: Uuid,
    pub state: TerminalState,
    pub cols: u16,
    pub rows: u16,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub output_start_cursor: u64,
    pub output_end_cursor: u64,
    pub acked_cursor: u64,
    pub output_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRead {
    pub start_cursor: u64,
    pub next_cursor: u64,
    pub bytes: Vec<u8>,
    pub record: TerminalRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDecision {
    Execute,
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDecision {
    Execute,
    Applied(usize),
}

pub enum MutationDecision {
    Execute,
    Cached(ResponseEnvelope),
}

#[derive(Debug, Clone)]
pub struct TerminalJournal {
    connection: Arc<Mutex<Connection>>,
}

impl TerminalJournal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(JournalError::Corrupt);
        }
        connection.execute_batch(
            "PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA wal_autocheckpoint=1000;
             CREATE TABLE IF NOT EXISTS terminals (
               terminal_id TEXT PRIMARY KEY NOT NULL,
               workspace_id TEXT NOT NULL,
               start_fingerprint BLOB NOT NULL CHECK(length(start_fingerprint) = 32),
               state TEXT NOT NULL,
               cols INTEGER NOT NULL CHECK(cols BETWEEN 1 AND 65535),
               rows INTEGER NOT NULL CHECK(rows BETWEEN 1 AND 65535),
               exit_code INTEGER,
               reason TEXT,
               output_start INTEGER NOT NULL DEFAULT 0 CHECK(output_start >= 0),
               output_end INTEGER NOT NULL DEFAULT 0 CHECK(output_end >= output_start),
               ack_cursor INTEGER NOT NULL DEFAULT 0 CHECK(ack_cursor >= 0),
               output_bytes INTEGER NOT NULL DEFAULT 0 CHECK(output_bytes >= 0),
               output_complete INTEGER NOT NULL DEFAULT 1 CHECK(output_complete IN (0, 1)),
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS output_chunks (
               terminal_id TEXT NOT NULL REFERENCES terminals(terminal_id) ON DELETE CASCADE,
               start_cursor INTEGER NOT NULL CHECK(start_cursor >= 0),
               end_cursor INTEGER NOT NULL CHECK(end_cursor > start_cursor),
               data BLOB NOT NULL CHECK(length(data) <= 32768),
               PRIMARY KEY (terminal_id, start_cursor)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS terminal_inputs (
               terminal_id TEXT NOT NULL REFERENCES terminals(terminal_id) ON DELETE CASCADE,
               input_id TEXT NOT NULL,
               fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32),
               status TEXT NOT NULL CHECK(status IN ('PENDING', 'APPLIED', 'UNKNOWN')),
               byte_count INTEGER,
               updated_at INTEGER NOT NULL,
               PRIMARY KEY (terminal_id, input_id)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS protocol_mutations (
               request_id TEXT PRIMARY KEY NOT NULL,
               terminal_id TEXT NOT NULL,
               kind TEXT NOT NULL,
               fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32),
               status TEXT NOT NULL CHECK(status IN ('PENDING', 'COMPLETE')),
               response BLOB,
               updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS terminals_workspace_state
               ON terminals(workspace_id, state);",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, JournalError> {
        self.connection.lock().map_err(|_| JournalError::Poisoned)
    }

    pub fn begin_start(
        &self,
        terminal_id: Uuid,
        workspace_id: Uuid,
        fingerprint: [u8; 32],
        cols: u16,
        rows: u16,
    ) -> Result<StartDecision, JournalError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, Vec<u8>)> = transaction
            .query_row(
                "SELECT workspace_id, start_fingerprint FROM terminals WHERE terminal_id = ?1",
                [terminal_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_workspace, stored_fingerprint)) = existing {
            if stored_workspace != workspace_id.to_string() || stored_fingerprint != fingerprint {
                return Err(JournalError::Conflict);
            }
            transaction.commit()?;
            return Ok(StartDecision::Existing);
        }
        let now = unix_millis();
        transaction.execute(
            "INSERT INTO terminals
             (terminal_id, workspace_id, start_fingerprint, state, cols, rows, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'STARTING', ?4, ?5, ?6, ?6)",
            params![
                terminal_id.to_string(),
                workspace_id.to_string(),
                fingerprint.as_slice(),
                i64::from(cols),
                i64::from(rows),
                now
            ],
        )?;
        transaction.commit()?;
        Ok(StartDecision::Execute)
    }

    pub fn set_running(&self, terminal_id: Uuid) -> Result<(), JournalError> {
        self.update_state(terminal_id, TerminalState::Running, None, None, None)
    }

    pub fn set_state(
        &self,
        terminal_id: Uuid,
        state: TerminalState,
        exit_code: Option<i32>,
        reason: Option<&str>,
        output_complete: Option<bool>,
    ) -> Result<(), JournalError> {
        self.update_state(terminal_id, state, exit_code, reason, output_complete)
    }

    fn update_state(
        &self,
        terminal_id: Uuid,
        state: TerminalState,
        exit_code: Option<i32>,
        reason: Option<&str>,
        output_complete: Option<bool>,
    ) -> Result<(), JournalError> {
        let changed = self.lock()?.execute(
            "UPDATE terminals SET state = ?2, exit_code = ?3, reason = ?4,
             output_complete = COALESCE(?5, output_complete), updated_at = ?6
             WHERE terminal_id = ?1",
            params![
                terminal_id.to_string(),
                state_name(state),
                exit_code,
                reason,
                output_complete.map(i64::from),
                unix_millis()
            ],
        )?;
        if changed == 0 {
            Err(JournalError::NotFound)
        } else {
            Ok(())
        }
    }

    pub fn set_size(&self, terminal_id: Uuid, cols: u16, rows: u16) -> Result<(), JournalError> {
        let changed = self.lock()?.execute(
            "UPDATE terminals SET cols = ?2, rows = ?3, updated_at = ?4 WHERE terminal_id = ?1",
            params![
                terminal_id.to_string(),
                i64::from(cols),
                i64::from(rows),
                unix_millis()
            ],
        )?;
        if changed == 0 {
            Err(JournalError::NotFound)
        } else {
            Ok(())
        }
    }

    pub fn append_output(&self, terminal_id: Uuid, bytes: &[u8]) -> Result<usize, JournalError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut cursor, total, complete): (i64, i64, bool) = transaction.query_row(
            "SELECT output_end, output_bytes, output_complete FROM terminals WHERE terminal_id = ?1",
            [terminal_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if !complete || total >= MAX_TERMINAL_OUTPUT_BYTES as i64 {
            transaction.execute(
                "UPDATE terminals SET output_complete = 0, updated_at = ?2 WHERE terminal_id = ?1",
                params![terminal_id.to_string(), unix_millis()],
            )?;
            transaction.commit()?;
            return Ok(0);
        }
        let available = (MAX_TERMINAL_OUTPUT_BYTES as i64 - total) as usize;
        let accepted = bytes.len().min(available);
        for chunk in bytes[..accepted].chunks(MAX_TERMINAL_OUTPUT_CHUNK_BYTES) {
            let end = cursor + i64::try_from(chunk.len()).map_err(|_| JournalError::Corrupt)?;
            transaction.execute(
                "INSERT INTO output_chunks (terminal_id, start_cursor, end_cursor, data)
                 VALUES (?1, ?2, ?3, ?4)",
                params![terminal_id.to_string(), cursor, end, chunk],
            )?;
            cursor = end;
        }
        let overflowed = accepted != bytes.len();
        transaction.execute(
            "UPDATE terminals SET output_end = ?2, output_bytes = output_bytes + ?3,
             output_complete = CASE WHEN ?4 THEN 0 ELSE output_complete END, updated_at = ?5
             WHERE terminal_id = ?1",
            params![
                terminal_id.to_string(),
                cursor,
                i64::try_from(accepted).map_err(|_| JournalError::Corrupt)?,
                overflowed,
                unix_millis()
            ],
        )?;
        transaction.commit()?;
        Ok(accepted)
    }

    pub fn read_output(
        &self,
        terminal_id: Uuid,
        after_cursor: u64,
        max_bytes: usize,
    ) -> Result<OutputRead, JournalError> {
        let connection = self.lock()?;
        let record = query_record(&connection, terminal_id)?;
        if after_cursor > record.output_end_cursor {
            return Err(JournalError::Conflict);
        }
        let start = after_cursor.max(record.output_start_cursor);
        let mut statement = connection.prepare(
            "SELECT start_cursor, end_cursor, data FROM output_chunks
             WHERE terminal_id = ?1 AND end_cursor > ?2 ORDER BY start_cursor",
        )?;
        let mut rows = statement.query(params![terminal_id.to_string(), to_i64(start)?])?;
        let mut bytes = Vec::with_capacity(max_bytes.min(MAX_TERMINAL_OUTPUT_CHUNK_BYTES));
        let mut next = start;
        while bytes.len() < max_bytes {
            let Some(row) = rows.next()? else { break };
            let chunk_start = to_u64(row.get::<_, i64>(0)?)?;
            let chunk_end = to_u64(row.get::<_, i64>(1)?)?;
            let data: Vec<u8> = row.get(2)?;
            if chunk_end - chunk_start != data.len() as u64
                || data.len() > MAX_TERMINAL_OUTPUT_CHUNK_BYTES
            {
                return Err(JournalError::Corrupt);
            }
            let offset = usize::try_from(next.saturating_sub(chunk_start))
                .map_err(|_| JournalError::Corrupt)?;
            if offset >= data.len() {
                continue;
            }
            let count = (max_bytes - bytes.len()).min(data.len() - offset);
            bytes.extend_from_slice(&data[offset..offset + count]);
            next = next
                .checked_add(count as u64)
                .ok_or(JournalError::Corrupt)?;
        }
        Ok(OutputRead {
            start_cursor: start,
            next_cursor: next,
            bytes,
            record,
        })
    }

    pub fn ack_output(&self, terminal_id: Uuid, cursor: u64) -> Result<u64, JournalError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (output_end, old_ack): (i64, i64) = transaction.query_row(
            "SELECT output_end, ack_cursor FROM terminals WHERE terminal_id = ?1",
            [terminal_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let cursor = to_i64(cursor)?;
        if cursor > output_end {
            return Err(JournalError::Conflict);
        }
        let ack = cursor.max(old_ack);
        transaction.execute(
            "DELETE FROM output_chunks WHERE terminal_id = ?1 AND end_cursor <= ?2",
            params![terminal_id.to_string(), ack],
        )?;
        transaction.execute(
            "UPDATE terminals SET ack_cursor = ?2, output_start = MAX(output_start, ?2), updated_at = ?3
             WHERE terminal_id = ?1",
            params![terminal_id.to_string(), ack, unix_millis()],
        )?;
        transaction.commit()?;
        to_u64(ack)
    }

    pub fn inspect(&self, terminal_id: Uuid) -> Result<TerminalRecord, JournalError> {
        let connection = self.lock()?;
        query_record(&connection, terminal_id)
    }

    pub fn begin_input(
        &self,
        terminal_id: Uuid,
        input_id: Uuid,
        fingerprint: [u8; 32],
    ) -> Result<InputDecision, JournalError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Vec<u8>, String, Option<i64>)> = transaction
            .query_row(
                "SELECT fingerprint, status, byte_count FROM terminal_inputs
                 WHERE terminal_id = ?1 AND input_id = ?2",
                params![terminal_id.to_string(), input_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((stored, status, byte_count)) = existing {
            if stored != fingerprint {
                return Err(JournalError::Conflict);
            }
            transaction.commit()?;
            return match status.as_str() {
                "APPLIED" => Ok(InputDecision::Applied(
                    usize::try_from(byte_count.ok_or(JournalError::Corrupt)?)
                        .map_err(|_| JournalError::Corrupt)?,
                )),
                "PENDING" | "UNKNOWN" => Err(JournalError::OutcomeUnknown),
                _ => Err(JournalError::Corrupt),
            };
        }
        transaction.execute(
            "INSERT INTO terminal_inputs
             (terminal_id, input_id, fingerprint, status, updated_at)
             VALUES (?1, ?2, ?3, 'PENDING', ?4)",
            params![
                terminal_id.to_string(),
                input_id.to_string(),
                fingerprint.as_slice(),
                unix_millis()
            ],
        )?;
        transaction.commit()?;
        Ok(InputDecision::Execute)
    }

    pub fn complete_input(
        &self,
        terminal_id: Uuid,
        input_id: Uuid,
        bytes: usize,
    ) -> Result<(), JournalError> {
        let changed = self.lock()?.execute(
            "UPDATE terminal_inputs SET status = 'APPLIED', byte_count = ?3, updated_at = ?4
             WHERE terminal_id = ?1 AND input_id = ?2 AND status = 'PENDING'",
            params![
                terminal_id.to_string(),
                input_id.to_string(),
                i64::try_from(bytes).map_err(|_| JournalError::Corrupt)?,
                unix_millis()
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(JournalError::OutcomeUnknown)
        }
    }

    pub fn mark_input_unknown(
        &self,
        terminal_id: Uuid,
        input_id: Uuid,
    ) -> Result<(), JournalError> {
        self.lock()?.execute(
            "UPDATE terminal_inputs SET status = 'UNKNOWN', updated_at = ?3
             WHERE terminal_id = ?1 AND input_id = ?2 AND status = 'PENDING'",
            params![terminal_id.to_string(), input_id.to_string(), unix_millis()],
        )?;
        Ok(())
    }

    pub fn begin_mutation(
        &self,
        request_id: Uuid,
        terminal_id: Uuid,
        kind: &str,
        fingerprint: [u8; 32],
    ) -> Result<MutationDecision, JournalError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<StoredMutation> = transaction
            .query_row(
                "SELECT terminal_id, kind, fingerprint, status, response
                 FROM protocol_mutations WHERE request_id = ?1",
                [request_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((stored_terminal, stored_kind, stored_fingerprint, status, response)) = existing
        {
            if stored_terminal != terminal_id.to_string()
                || stored_kind != kind
                || stored_fingerprint != fingerprint
            {
                return Err(JournalError::Conflict);
            }
            transaction.commit()?;
            return match status.as_str() {
                "COMPLETE" => Ok(MutationDecision::Cached(
                    serde_json::from_slice(&response.ok_or(JournalError::Corrupt)?)
                        .map_err(|_| JournalError::Corrupt)?,
                )),
                "PENDING" => Err(JournalError::OutcomeUnknown),
                _ => Err(JournalError::Corrupt),
            };
        }
        transaction.execute(
            "INSERT INTO protocol_mutations
             (request_id, terminal_id, kind, fingerprint, status, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'PENDING', ?5)",
            params![
                request_id.to_string(),
                terminal_id.to_string(),
                kind,
                fingerprint.as_slice(),
                unix_millis()
            ],
        )?;
        transaction.commit()?;
        Ok(MutationDecision::Execute)
    }

    pub fn complete_mutation(
        &self,
        request_id: Uuid,
        response: &ResponseEnvelope,
    ) -> Result<(), JournalError> {
        let encoded = serde_json::to_vec(response).map_err(|_| JournalError::Corrupt)?;
        let changed = self.lock()?.execute(
            "UPDATE protocol_mutations SET status = 'COMPLETE', response = ?2, updated_at = ?3
             WHERE request_id = ?1 AND status = 'PENDING'",
            params![request_id.to_string(), encoded, unix_millis()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(JournalError::OutcomeUnknown)
        }
    }

    pub fn reconcile_restart(&self) -> Result<Vec<Uuid>, JournalError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let terminal_ids = {
            let mut statement = transaction.prepare(
                "SELECT terminal_id FROM terminals
                 WHERE state IN ('STARTING', 'RUNNING', 'TERMINATING', 'UNRECOVERABLE')",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| Uuid::parse_str(&row?).map_err(|_| rusqlite::Error::InvalidQuery))
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction.execute(
            "UPDATE terminals SET state = 'LOST', reason = 'daemon_restart_pty_lost',
             output_complete = 0, updated_at = ?1
             WHERE state IN ('STARTING', 'RUNNING', 'TERMINATING', 'UNRECOVERABLE')",
            [unix_millis()],
        )?;
        transaction.execute(
            "UPDATE terminal_inputs SET status = 'UNKNOWN', updated_at = ?1 WHERE status = 'PENDING'",
            [unix_millis()],
        )?;
        transaction.commit()?;
        Ok(terminal_ids)
    }

    #[cfg(test)]
    pub(crate) fn set_output_bytes_for_test(
        &self,
        terminal_id: Uuid,
        bytes: u64,
    ) -> Result<(), JournalError> {
        self.lock()?.execute(
            "UPDATE terminals SET output_bytes = ?2 WHERE terminal_id = ?1",
            params![terminal_id.to_string(), to_i64(bytes)?],
        )?;
        Ok(())
    }
}

fn query_record(
    connection: &Connection,
    terminal_id: Uuid,
) -> Result<TerminalRecord, JournalError> {
    connection
        .query_row(
            "SELECT workspace_id, state, cols, rows, exit_code, reason,
             output_start, output_end, ack_cursor, output_complete
             FROM terminals WHERE terminal_id = ?1",
            [terminal_id.to_string()],
            |row| {
                let workspace: String = row.get(0)?;
                let state: String = row.get(1)?;
                Ok((
                    workspace,
                    state,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            },
        )
        .optional()?
        .ok_or(JournalError::NotFound)
        .and_then(
            |(workspace, state, cols, rows, exit_code, reason, start, end, ack, complete)| {
                Ok(TerminalRecord {
                    terminal_id,
                    workspace_id: Uuid::parse_str(&workspace).map_err(|_| JournalError::Corrupt)?,
                    state: parse_state(&state)?,
                    cols: u16::try_from(cols).map_err(|_| JournalError::Corrupt)?,
                    rows: u16::try_from(rows).map_err(|_| JournalError::Corrupt)?,
                    exit_code,
                    reason,
                    output_start_cursor: to_u64(start)?,
                    output_end_cursor: to_u64(end)?,
                    acked_cursor: to_u64(ack)?,
                    output_complete: complete,
                })
            },
        )
}

fn state_name(state: TerminalState) -> &'static str {
    match state {
        TerminalState::Running => "RUNNING",
        TerminalState::Terminating => "TERMINATING",
        TerminalState::Unrecoverable => "UNRECOVERABLE",
        TerminalState::Lost => "LOST",
        TerminalState::Exited => "EXITED",
        TerminalState::Terminated => "TERMINATED",
    }
}

fn parse_state(state: &str) -> Result<TerminalState, JournalError> {
    match state {
        "STARTING" | "RUNNING" => Ok(TerminalState::Running),
        "TERMINATING" => Ok(TerminalState::Terminating),
        "UNRECOVERABLE" => Ok(TerminalState::Unrecoverable),
        "LOST" => Ok(TerminalState::Lost),
        "EXITED" => Ok(TerminalState::Exited),
        "TERMINATED" => Ok(TerminalState::Terminated),
        _ => Err(JournalError::Corrupt),
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn to_i64(value: u64) -> Result<i64, JournalError> {
    i64::try_from(value).map_err(|_| JournalError::Corrupt)
}

fn to_u64(value: i64) -> Result<u64, JournalError> {
    u64::try_from(value).map_err(|_| JournalError::Corrupt)
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, thread};

    use gobrowse_core::sandbox::{
        NetworkPolicy, ResourceLimits, SANDBOX_PROTOCOL_VERSION, SandboxResult,
    };

    use super::*;

    struct TestJournal {
        path: std::path::PathBuf,
        journal: TerminalJournal,
    }

    impl TestJournal {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("gobrowse-journal-{}.sqlite3", Uuid::new_v4()));
            let journal = TerminalJournal::open(&path).unwrap();
            Self { path, journal }
        }

        fn terminal(&self) -> Uuid {
            let terminal_id = Uuid::new_v4();
            self.journal
                .begin_start(terminal_id, Uuid::new_v4(), [7; 32], 80, 24)
                .unwrap();
            self.journal.set_running(terminal_id).unwrap();
            terminal_id
        }
    }

    impl Drop for TestJournal {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = fs::remove_file(format!("{}{}", self.path.display(), suffix));
            }
        }
    }

    #[test]
    fn cursors_ack_and_reopen_do_not_duplicate_output() {
        let test = TestJournal::new();
        let terminal_id = test.terminal();
        test.journal
            .append_output(terminal_id, b"first-second")
            .unwrap();
        let first = test.journal.read_output(terminal_id, 0, 5).unwrap();
        assert_eq!(first.bytes, b"first");
        let second = test
            .journal
            .read_output(terminal_id, first.next_cursor, 64)
            .unwrap();
        assert_eq!(second.bytes, b"-second");
        assert_eq!(
            test.journal
                .ack_output(terminal_id, first.next_cursor)
                .unwrap(),
            5
        );

        let reopened = TerminalJournal::open(&test.path).unwrap();
        let after_reopen = reopened
            .read_output(terminal_id, first.next_cursor, 64)
            .unwrap();
        assert_eq!(after_reopen.bytes, b"-second");
        assert_eq!(after_reopen.start_cursor, first.next_cursor);
    }

    #[test]
    fn restart_marks_pty_lost_and_pending_input_unknown() {
        let test = TestJournal::new();
        let terminal_id = test.terminal();
        let input_id = Uuid::new_v4();
        test.journal
            .begin_input(terminal_id, input_id, [9; 32])
            .unwrap();
        drop(test.journal.clone());

        let reopened = TerminalJournal::open(&test.path).unwrap();
        assert_eq!(reopened.reconcile_restart().unwrap(), [terminal_id]);
        let record = reopened.inspect(terminal_id).unwrap();
        assert_eq!(record.state, TerminalState::Lost);
        assert!(!record.output_complete);
        assert!(matches!(
            reopened.begin_input(terminal_id, input_id, [9; 32]),
            Err(JournalError::OutcomeUnknown)
        ));
    }

    #[test]
    fn input_ids_and_pending_mutation_ambiguity_survive_reopen() {
        let test = TestJournal::new();
        let terminal_id = test.terminal();
        let input_id = Uuid::new_v4();
        test.journal
            .begin_input(terminal_id, input_id, [4; 32])
            .unwrap();
        test.journal
            .complete_input(terminal_id, input_id, 17)
            .unwrap();
        let request_id = Uuid::new_v4();
        assert!(matches!(
            test.journal
                .begin_mutation(request_id, terminal_id, "interrupt", [6; 32])
                .unwrap(),
            MutationDecision::Execute
        ));

        let reopened = TerminalJournal::open(&test.path).unwrap();
        assert_eq!(
            reopened
                .begin_input(terminal_id, input_id, [4; 32])
                .unwrap(),
            InputDecision::Applied(17)
        );
        assert!(matches!(
            reopened.begin_mutation(request_id, terminal_id, "interrupt", [6; 32]),
            Err(JournalError::OutcomeUnknown)
        ));
        assert!(matches!(
            reopened.begin_input(terminal_id, input_id, [5; 32]),
            Err(JournalError::Conflict)
        ));
    }

    #[test]
    fn output_limit_is_durable_and_marks_history_incomplete() {
        let test = TestJournal::new();
        let terminal_id = test.terminal();
        test.journal
            .lock()
            .unwrap()
            .execute(
                "UPDATE terminals SET output_bytes = ?2 WHERE terminal_id = ?1",
                params![
                    terminal_id.to_string(),
                    MAX_TERMINAL_OUTPUT_BYTES as i64 - 10
                ],
            )
            .unwrap();
        assert_eq!(
            test.journal.append_output(terminal_id, &[1; 20]).unwrap(),
            10
        );
        let record = test.journal.inspect(terminal_id).unwrap();
        assert_eq!(record.output_end_cursor, 10);
        assert!(!record.output_complete);
        assert_eq!(
            test.journal.append_output(terminal_id, b"ignored").unwrap(),
            0
        );
    }

    #[test]
    fn concurrent_output_resize_and_protocol_mutations_remain_consistent() {
        let test = TestJournal::new();
        let terminal_id = test.terminal();
        let journal = Arc::new(test.journal.clone());
        let mut workers = Vec::new();
        for index in 0_u8..8 {
            let journal = Arc::clone(&journal);
            workers.push(thread::spawn(move || {
                journal.append_output(terminal_id, &[index; 1024]).unwrap();
                journal
                    .set_size(terminal_id, 80 + u16::from(index), 24 + u16::from(index))
                    .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let output = journal.read_output(terminal_id, 0, 8 * 1024).unwrap();
        assert_eq!(output.bytes.len(), 8 * 1024);
        assert_eq!(output.next_cursor, 8 * 1024);

        let request_id = Uuid::new_v4();
        assert!(matches!(
            journal
                .begin_mutation(request_id, terminal_id, "resize", [3; 32])
                .unwrap(),
            MutationDecision::Execute
        ));
        let response = ResponseEnvelope {
            version: SANDBOX_PROTOCOL_VERSION,
            request_id,
            result: Ok(SandboxResult::Started {
                terminal_id,
                limits: ResourceLimits {
                    cpu_millis: 1,
                    memory_bytes: 1,
                    writable_storage_bytes: 1,
                    pids: 1,
                    execution_seconds: 1,
                },
                network_policy: NetworkPolicy::None,
            }),
        };
        journal.complete_mutation(request_id, &response).unwrap();
        assert!(matches!(
            TerminalJournal::open(&test.path)
                .unwrap()
                .begin_mutation(request_id, terminal_id, "resize", [3; 32])
                .unwrap(),
            MutationDecision::Cached(cached) if cached == response
        ));
    }
}
