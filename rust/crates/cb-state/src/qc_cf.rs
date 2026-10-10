//! QC CF: stores quorum certificates keyed by `height`. The QC for height H
//! is what proves block H was finalized by 2/3 of validators; block H+1 carries
//! that QC in its `parent_qc` slot. Holding QCs in a dedicated CF makes it
//! cheap for the leader to fetch the parent QC when proposing the next block,
//! and lets pruning (CF_BLOCKS) and finality (CF_QC) scale independently.

use crate::db::{Database, CF_QC};

pub struct Qcs<'a> {
    db: &'a Database,
}

impl<'a> Qcs<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, height: u64, bytes: &[u8]) {
        self.db
            .put(CF_QC, &height.to_be_bytes(), bytes)
            .expect("rocks");
    }

    pub fn get(&self, height: u64) -> Option<Vec<u8>> {
        self.db.get(CF_QC, &height.to_be_bytes()).expect("rocks")
    }

    pub fn delete(&self, height: u64) {
        self.db.delete(CF_QC, &height.to_be_bytes()).expect("rocks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use tempfile::tempdir;

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let qcs = Qcs::new(&db);
        assert_eq!(qcs.get(7), None);
        qcs.put(7, b"qc-bytes");
        assert_eq!(qcs.get(7).as_deref(), Some(&b"qc-bytes"[..]));
    }

    #[test]
    fn delete_removes() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let qcs = Qcs::new(&db);
        qcs.put(1, b"x");
        qcs.delete(1);
        assert_eq!(qcs.get(1), None);
    }
}
