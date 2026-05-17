//! Volume-split layout for a PAR2 recovery set.
//!
//! A recovery set is published as an index file (`<base>.par2`, no recovery
//! data) plus a series of recovery volume files
//! (`<base>.vol<first>+<count>.par2`). Recovery blocks are distributed across
//! volumes with exponentially growing counts (1, 2, 4, 8, …), so a downloader
//! can fetch just as many recovery blocks as it needs.

/// One recovery volume: a contiguous run of recovery block exponents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeChunk {
    /// Exponent of the first recovery block in this volume.
    pub first: u32,
    /// Number of recovery blocks in this volume.
    pub count: u32,
}

/// Distribute `total` recovery blocks across volumes with exponentially
/// growing sizes (1, 2, 4, 8, …); the last volume takes the remainder.
pub fn plan_volumes(total: u32) -> Vec<VolumeChunk> {
    let mut chunks = Vec::new();
    let mut first = 0u32;
    let mut size = 1u32;
    while first < total {
        let count = size.min(total - first);
        chunks.push(VolumeChunk { first, count });
        first += count;
        size = size.saturating_mul(2);
    }
    chunks
}

/// File name of the index file, e.g. `movie.mkv.par2`.
pub fn index_name(base: &str) -> String {
    format!("{base}.par2")
}

/// File name of a recovery volume, e.g. `movie.mkv.vol000+002.par2`.
pub fn volume_name(base: &str, chunk: VolumeChunk) -> String {
    format!("{base}.vol{:03}+{:03}.par2", chunk.first, chunk.count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(first: u32, count: u32) -> VolumeChunk {
        VolumeChunk { first, count }
    }

    #[test]
    fn plan_volumes_grows_exponentially() {
        assert_eq!(plan_volumes(0), vec![]);
        assert_eq!(plan_volumes(1), vec![chunk(0, 1)]);
        assert_eq!(plan_volumes(7), vec![chunk(0, 1), chunk(1, 2), chunk(3, 4)]);
        assert_eq!(
            plan_volumes(10),
            vec![chunk(0, 1), chunk(1, 2), chunk(3, 4), chunk(7, 3)]
        );
    }

    #[test]
    fn planned_chunks_cover_every_block_once() {
        let chunks = plan_volumes(100);
        let total: u32 = chunks.iter().map(|c| c.count).sum();
        assert_eq!(total, 100);
        let mut next = 0;
        for c in &chunks {
            assert_eq!(c.first, next);
            next += c.count;
        }
    }

    #[test]
    fn file_names_follow_the_convention() {
        assert_eq!(index_name("movie.mkv"), "movie.mkv.par2");
        assert_eq!(
            volume_name("movie.mkv", chunk(0, 2)),
            "movie.mkv.vol000+002.par2"
        );
        assert_eq!(
            volume_name("movie.mkv", chunk(15, 16)),
            "movie.mkv.vol015+016.par2"
        );
    }
}
