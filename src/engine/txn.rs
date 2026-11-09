//! Uncommitted changes of the current transaction, with statement-level undo.

use std::collections::BTreeMap;

use crate::types::Value;

/// Per-table pending writes: pk -> new row, or `None` for a delete.
pub type Changes = BTreeMap<i64, Option<Vec<Value>>>;

/// (table, pk, previous overlay entry); `None` means the key was not in the
/// overlay before this statement.
type UndoEntry = (u32, i64, Option<Option<Vec<Value>>>);

#[derive(Default)]
pub struct Txn {
    /// Inside BEGIN ... COMMIT/ROLLBACK. Otherwise every statement commits
    /// on its own.
    pub explicit: bool,
    changes: BTreeMap<u32, Changes>,
    /// Writes of the current statement, for undo.
    undo: Vec<UndoEntry>,
}

impl Txn {
    pub fn changes(&self, table: u32) -> Option<&Changes> {
        self.changes.get(&table).filter(|c| !c.is_empty())
    }

    pub fn write(&mut self, table: u32, pk: i64, row: Option<Vec<Value>>) {
        let prev = self.changes.entry(table).or_default().insert(pk, row);
        self.undo.push((table, pk, prev));
    }

    pub fn begin_statement(&mut self) {
        self.undo.clear();
    }

    /// Reverts every write of the current statement.
    pub fn undo_statement(&mut self) {
        while let Some((table, pk, prev)) = self.undo.pop() {
            let changes = self.changes.entry(table).or_default();
            match prev {
                Some(p) => {
                    changes.insert(pk, p);
                }
                None => {
                    changes.remove(&pk);
                }
            }
        }
    }

    pub fn take_changes(&mut self) -> BTreeMap<u32, Changes> {
        self.undo.clear();
        std::mem::take(&mut self.changes)
    }

    pub fn clear(&mut self) {
        self.changes.clear();
        self.undo.clear();
    }
}
