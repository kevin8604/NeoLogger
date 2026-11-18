use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::source::{LineRef, LogDoc};

/// Stable k-way merge. Ties are broken by the order in which documents were supplied,
/// then by line number, so equal timestamps keep each file's internal order.
pub fn merge_docs(docs: &[&LogDoc]) -> Vec<LineRef> {
    let total: usize = docs.iter().map(|d| d.len()).sum();
    let mut out = Vec::with_capacity(total);
    let mut heap: BinaryHeap<Reverse<(i64, usize, u32)>> = BinaryHeap::new();

    for (idx, doc) in docs.iter().enumerate() {
        if doc.len() > 0 {
            heap.push(Reverse((doc.time_at(0).unwrap_or(0), idx, 0)));
        }
    }

    while let Some(Reverse((_, idx, line))) = heap.pop() {
        let doc = docs[idx];
        out.push(LineRef { doc: doc.id, line });
        let next = line as usize + 1;
        if next < doc.len() {
            heap.push(Reverse((doc.time_at(next).unwrap_or(0), idx, next as u32)));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: u32, times: &[i64]) -> LogDoc {
        let mut d = LogDoc::from_string(
            id,
            format!("d{id}.log"),
            times.iter().map(|t| format!("t={t}\n")).collect::<String>(),
        );
        d.set_times(times.to_vec());
        d
    }

    #[test]
    fn merges_by_timestamp_across_docs() {
        let a = doc(0, &[1, 3, 5]);
        let b = doc(1, &[2, 3, 6]);
        let merged = merge_docs(&[&a, &b]);
        let pairs: Vec<(u32, u32)> = merged.iter().map(|r| (r.doc, r.line)).collect();
        assert_eq!(pairs, vec![(0, 0), (1, 0), (0, 1), (1, 1), (0, 2), (1, 2)]);
    }

    #[test]
    fn keeps_document_order_on_ties() {
        let a = doc(0, &[1, 1]);
        let b = doc(1, &[1, 1]);
        let merged = merge_docs(&[&a, &b]);
        assert_eq!(
            merged.iter().map(|r| r.doc).collect::<Vec<_>>(),
            vec![0, 0, 1, 1]
        );
    }

    #[test]
    fn handles_empty_documents() {
        let a = LogDoc::from_string(0, "empty.log", "");
        let b = doc(1, &[1]);
        assert_eq!(merge_docs(&[&a, &b]).len(), 1);
    }
}
