/// Compact set of row indices. Filtered views keep one bit per parent row and
/// answer `select(k)` with a rank table, so a million-row filter costs ~125 KB
/// instead of 8 MB of `LineRef`s.
#[derive(Clone, Debug, Default)]
pub struct Bitmap {
    words: Vec<u64>,
    ranks: Vec<u32>,
    len: usize,
    ones: usize,
}

impl Bitmap {
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            ranks: Vec::new(),
            len,
            ones: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn count_ones(&self) -> usize {
        self.ones
    }

    pub fn set(&mut self, idx: usize) {
        if idx >= self.len {
            return;
        }
        self.words[idx / 64] |= 1 << (idx % 64);
    }

    pub fn get(&self, idx: usize) -> bool {
        idx < self.len && self.words[idx / 64] & (1 << (idx % 64)) != 0
    }

    /// Builds the rank table. Must be called once after all `set` calls.
    pub fn finish(&mut self) {
        self.ranks = Vec::with_capacity(self.words.len() + 1);
        self.ranks.push(0);
        let mut total = 0u32;
        for word in &self.words {
            total += word.count_ones();
            self.ranks.push(total);
        }
        self.ones = total as usize;
    }

    fn rank_before_word(&self, word: usize) -> u32 {
        self.ranks.get(word).copied().unwrap_or(self.ones as u32)
    }

    /// Index of the `k`-th set bit (0-based).
    pub fn select(&self, k: usize) -> Option<usize> {
        if k >= self.ones {
            return None;
        }
        let k = k as u32;
        let word = self.ranks.partition_point(|&r| r <= k).saturating_sub(1);
        let mut remaining = k - self.rank_before_word(word);
        let mut bits = *self.words.get(word)?;
        while remaining > 0 {
            bits &= bits - 1;
            remaining -= 1;
        }
        Some(word * 64 + bits.trailing_zeros() as usize)
    }

    /// Number of set bits before `idx`.
    pub fn count_before(&self, idx: usize) -> usize {
        let idx = idx.min(self.len);
        let word = idx / 64;
        let mut count = self.rank_before_word(word) as usize;
        if word < self.words.len() {
            let mask = if idx % 64 == 0 {
                0
            } else {
                (1u64 << (idx % 64)) - 1
            };
            count += (self.words[word] & mask).count_ones() as usize;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_and_rank() {
        let mut bits = Bitmap::new(300);
        for idx in [0, 1, 63, 64, 65, 129, 299] {
            bits.set(idx);
        }
        bits.finish();
        assert_eq!(bits.count_ones(), 7);
        let selected: Vec<usize> = (0..bits.count_ones())
            .filter_map(|k| bits.select(k))
            .collect();
        assert_eq!(selected, vec![0, 1, 63, 64, 65, 129, 299]);
        assert_eq!(bits.count_before(64), 3);
        assert_eq!(bits.count_before(65), 4);
        assert_eq!(bits.count_before(300), 7);
        assert_eq!(bits.select(7), None);
    }

    #[test]
    fn empty_bitmap() {
        let mut bits = Bitmap::new(0);
        bits.finish();
        assert_eq!(bits.count_ones(), 0);
        assert_eq!(bits.select(0), None);
    }
}
