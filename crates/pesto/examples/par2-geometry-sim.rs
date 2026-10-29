//! Monte Carlo study of PAR2 slice geometry against lost Usenet articles.
//!
//! For each workload and slice strategy, it asks `parmesan::ops::
//! calculate_geometry` for the real slice size and recovery count, then
//! removes random articles and counts the damaged slices. A loss is
//! repairable when the damaged slices do not exceed the recovery blocks.
//! The reported capacity is the largest share of articles that can be lost
//! while at least 95 % of trials stay repairable.
//!
//! ```text
//! cargo run --release -p pesto-poster --example par2-geometry-sim
//! ```
//!
//! The method and the results are discussed in `docs/par2-geometry.md`.

use std::collections::HashSet;
use std::path::PathBuf;

use parmesan::ops::{calculate_geometry, CreateOptions, InputFile};

const ARTICLE: usize = 768_000;
const PAR2_PCT: u8 = 10;
const TRIALS: usize = 200;
const SUCCESS: f64 = 0.95;
const BURST: usize = 20;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

struct Workload {
    name: &'static str,
    sizes: Vec<u64>,
}

fn workloads() -> Vec<Workload> {
    let mut mixed = vec![2 * GIB, 48 * MIB, 8 * KIB, 420 * KIB, 1200 * KIB];
    mixed.extend(std::iter::repeat_n(64 * KIB, 24));
    vec![
        Workload {
            name: "movie-1080p (6 GiB)",
            sizes: vec![6 * GIB],
        },
        Workload {
            name: "movie-remux-4k (40 GiB)",
            sizes: vec![40 * GIB],
        },
        Workload {
            name: "season-pack (8 × 1200 MiB)",
            sizes: vec![1200 * MIB; 8],
        },
        Workload {
            name: "rar volumes (6 × 500 MiB)",
            sizes: vec![500 * MIB; 6],
        },
        Workload {
            name: "mixed-folder (2 GiB + 28 small)",
            sizes: mixed,
        },
        Workload {
            name: "many-small (2000 × 256 KiB)",
            sizes: vec![256 * KIB; 2000],
        },
        Workload {
            name: "episode (1200 MiB)",
            sizes: vec![1200 * MIB],
        },
        Workload {
            name: "movie (2 GiB)",
            sizes: vec![2 * GIB],
        },
        Workload {
            name: "movie (3 GiB)",
            sizes: vec![3 * GIB],
        },
        Workload {
            name: "movie (4 GiB)",
            sizes: vec![4 * GIB],
        },
        Workload {
            name: "movie (15 GiB)",
            sizes: vec![15 * GIB],
        },
        Workload {
            name: "small release (300 MiB)",
            sizes: vec![300 * MIB],
        },
    ]
}

#[derive(Clone, Copy)]
enum Strategy {
    /// The geometry before article alignment: about 2000 slices of any
    /// multiple of 64 bytes.
    Unaligned,
    /// Slices aligned to the article size at a given `effort`.
    Aligned(f64),
    /// One slice per article.
    Article,
}

impl Strategy {
    fn label(self) -> String {
        match self {
            Strategy::Unaligned => "unaligned (old default)".into(),
            Strategy::Aligned(1.0) => "aligned (default)".into(),
            Strategy::Aligned(effort) => format!("aligned, effort {effort}"),
            Strategy::Article => "slice = article".into(),
        }
    }

    fn options(self) -> CreateOptions {
        let mut options = CreateOptions {
            recovery_pct: PAR2_PCT,
            ..CreateOptions::default()
        };
        match self {
            Strategy::Unaligned => {}
            Strategy::Aligned(effort) => {
                options.slice_unit = Some(ARTICLE);
                options.effort = effort;
            }
            Strategy::Article => options.slice_size = Some(ARTICLE),
        }
        options
    }
}

/// One article: the global slice indices it overlaps.
struct Article {
    first_slice: usize,
    last_slice: usize,
}

struct Layout {
    articles: Vec<Article>,
    /// Article index at which each file starts, plus the total at the end.
    file_starts: Vec<usize>,
    slice_size: usize,
    slices: usize,
    recovery: usize,
    padded: u64,
}

fn layout(sizes: &[u64], strategy: Strategy) -> Option<Layout> {
    let files: Vec<InputFile> = sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| InputFile {
            path: PathBuf::new(),
            display_name: i.to_string(),
            size,
        })
        .collect();
    let (slice_size, slices, recovery) = calculate_geometry(&files, &strategy.options()).ok()?;
    let s = slice_size as u64;
    let a = ARTICLE as u64;
    let mut articles = Vec::new();
    let mut file_starts = Vec::new();
    let mut slice_base = 0usize;
    let mut padded = 0u64;
    for &size in sizes {
        file_starts.push(articles.len());
        let mut offset = 0u64;
        while offset < size {
            let end = (offset + a).min(size);
            articles.push(Article {
                first_slice: slice_base + (offset / s) as usize,
                last_slice: slice_base + ((end - 1) / s) as usize,
            });
            offset = end;
        }
        let file_slices = size.div_ceil(s);
        slice_base += file_slices as usize;
        padded += file_slices * s;
    }
    file_starts.push(articles.len());
    Some(Layout {
        articles,
        file_starts,
        slice_size,
        slices,
        recovery,
        padded,
    })
}

/// splitmix64: small, seeded, good enough for sampling positions.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone, Copy)]
enum Loss {
    /// `n` distinct articles anywhere in the release.
    Scattered,
    /// `n` runs of `BURST` consecutive articles, clipped at file ends.
    Bursts,
}

fn lost_articles(layout: &Layout, loss: Loss, n: usize, rng: &mut Rng) -> HashSet<usize> {
    let total = layout.articles.len();
    let mut lost = HashSet::new();
    match loss {
        Loss::Scattered => {
            while lost.len() < n.min(total) {
                lost.insert(rng.below(total));
            }
        }
        Loss::Bursts => {
            for _ in 0..n {
                let start = rng.below(total);
                let file = layout.file_starts.partition_point(|&s| s <= start) - 1;
                let end = (start + BURST).min(layout.file_starts[file + 1]);
                lost.extend(start..end);
            }
        }
    }
    lost
}

/// Share of articles lost in trials where repair still succeeds, and whether
/// enough trials succeed.
fn trial(layout: &Layout, loss: Loss, n: usize, seed: u64) -> (f64, bool) {
    let mut rng = Rng(seed);
    let mut ok = 0usize;
    let mut lost_total = 0usize;
    for _ in 0..TRIALS {
        let lost = lost_articles(layout, loss, n, &mut rng);
        lost_total += lost.len();
        let mut damaged = HashSet::new();
        for &article in &lost {
            let a = &layout.articles[article];
            damaged.extend(a.first_slice..=a.last_slice);
        }
        if damaged.len() <= layout.recovery {
            ok += 1;
        }
    }
    let share = lost_total as f64 / TRIALS as f64 / layout.articles.len() as f64;
    (share, ok as f64 / TRIALS as f64 >= SUCCESS)
}

/// Largest loss (as a share of all articles) that stays repairable.
fn capacity(layout: &Layout, loss: Loss) -> f64 {
    let total = layout.articles.len();
    let (mut lo, mut hi) = (0usize, total);
    let mut best = 0.0;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let (share, repairable) = trial(layout, loss, mid, mid as u64 ^ 0x5EED);
        if repairable {
            lo = mid;
            best = share;
        } else {
            hi = mid - 1;
        }
    }
    best
}

/// Wall-clock create throughput on an i5-10400 (AVX2, 12 threads), from a
/// linear fit of `parmesan create` on a 4 GiB file in tmpfs: about 1.06 s
/// per GiB of reading and hashing plus 0.00996 s per GiB per recovery block.
fn mib_per_s(layout: &Layout, data: u64) -> f64 {
    let gib = layout.padded as f64 / GIB as f64;
    let seconds = gib * (1.06 + 0.00996 * layout.recovery as f64);
    data as f64 / MIB as f64 / seconds
}

fn main() {
    println!(
        "Article size {ARTICLE} B, {PAR2_PCT} % recovery, {TRIALS} trials, \
         capacity at ≥ {:.0} % repairable, bursts of {BURST} articles.\n",
        SUCCESS * 100.0
    );
    println!(
        "| workload | strategy | slice | slices | recovery | recovery bytes | create cost | est. i5-10400 | scattered loss | burst loss |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for workload in workloads() {
        let data: u64 = workload.sizes.iter().sum();
        let baseline = layout(&workload.sizes, Strategy::Unaligned)
            .map(|l| l.recovery as f64 * l.padded as f64);
        for strategy in [
            Strategy::Unaligned,
            Strategy::Aligned(1.0),
            Strategy::Aligned(2.0),
            Strategy::Article,
        ] {
            let Some(layout) = layout(&workload.sizes, strategy) else {
                println!(
                    "| {} | {} | — | over the 32768-slice limit | | | | | | |",
                    workload.name,
                    strategy.label()
                );
                continue;
            };
            let recovery_bytes = layout.recovery as u64 * layout.slice_size as u64;
            let cost = layout.recovery as f64 * layout.padded as f64;
            let relative = baseline.map_or(f64::NAN, |b| cost / b);
            println!(
                "| {} | {} | {} | {} | {} | {:.1} % | {:.2}× | {:.0} MiB/s | {:.2} % | {:.2} % |",
                workload.name,
                strategy.label(),
                layout.slice_size,
                layout.slices,
                layout.recovery,
                100.0 * recovery_bytes as f64 / data as f64,
                relative,
                mib_per_s(&layout, data),
                100.0 * capacity(&layout, Loss::Scattered),
                100.0 * capacity(&layout, Loss::Bursts),
            );
        }
    }
}
