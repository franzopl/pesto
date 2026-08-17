//! Standalone plain-TCP mock NNTP server for load, memory and benchmark runs.
//!
//! Accepts unlimited connections and ACKs enough of the protocol that a real
//! posting client — `pesto`, but equally `nyuu` or `ngPost` — can post against
//! it locally with many concurrent connections, without touching a real Usenet
//! server. It exists so the benchmark suite can measure poster throughput with
//! no network variance and no real account (see `bench/README.md`).
//!
//! Commands are matched case-insensitively: RFC 3977 §3.1 states that command
//! names are not case sensitive, and posters differ in practice (`ngPost`
//! sends lowercase `post`, `nyuu` and `pesto` send uppercase). Anything not
//! recognised gets `500`, which is what a real server would answer.
//!
//! Usage:
//!   `cargo run --release --example mock_nntp_server -- [PORT] [options]`
//!
//! Options:
//!   `--port N`            listen port (0 = pick a free one; also positional)
//!   `--latency-ms N`      delay every response by N ms, simulating server RTT
//!   `--post-latency-ms N` extra delay on the `240` that ends a POST
//!   `--stats-file PATH`   write a JSON summary here on SIGTERM/SIGINT
//!   `--save-dir PATH`     save every accepted article body under PATH
//!   `--drop-pct N`        reject N% of POSTs with `441`, to exercise retries
//!   `--miss-pct N`        answer `430` to N% of STATs, to exercise reposts
//!   `--quiet`             suppress the per-second progress line
//!
//! The listening address is printed as `listening on 127.0.0.1:<port>` on the
//! first line of stdout, so a harness can start it with `--port 0` and read
//! back the port it actually got instead of racing over a fixed one.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Read buffer per connection. Articles are ~768 KB, so a large buffer keeps
/// the body drain down to a handful of `read` syscalls: at the default
/// article size this server has to be able to sink several GB/s or it, and
/// not the poster under test, becomes the benchmark's bottleneck.
const READ_BUF: usize = 512 * 1024;

#[derive(Clone, Default)]
struct Opts {
    port: u16,
    latency: Option<Duration>,
    post_latency: Option<Duration>,
    stats_file: Option<PathBuf>,
    save_dir: Option<PathBuf>,
    drop_pct: u32,
    miss_pct: u32,
    quiet: bool,
}

#[derive(Default)]
struct Stats {
    connections: AtomicU64,
    articles: AtomicU64,
    article_bytes: AtomicU64,
    stats_cmds: AtomicU64,
    rejected: AtomicU64,
    missing: AtomicU64,
    /// Filename sequence for `--save-dir`, separate from `articles`.
    ///
    /// Naming a capture after the *current* article count races: two
    /// connections finishing at once both read the same value before either
    /// increments, and one silently overwrites the other's file. The wire
    /// round-trip check in `bench/suites/60-correctness.sh` then sees a hole
    /// in the reassembled file and reports a decoder bug that does not exist.
    saved: AtomicU64,
}

impl Stats {
    fn to_json(&self, elapsed_secs: f64) -> String {
        let articles = self.articles.load(Ordering::Relaxed);
        let bytes = self.article_bytes.load(Ordering::Relaxed);
        let per_s = if elapsed_secs > 0.0 {
            articles as f64 / elapsed_secs
        } else {
            0.0
        };
        let mibps = if elapsed_secs > 0.0 {
            (bytes as f64 / (1024.0 * 1024.0)) / elapsed_secs
        } else {
            0.0
        };
        format!(
            concat!(
                r#"{{"connections":{},"articles":{},"article_bytes":{},"stat_commands":{},"#,
                r#""rejected":{},"missing":{},"elapsed_secs":{:.3},"articles_per_s":{:.1},"#,
                r#""wire_mibps":{:.1}}}"#
            ),
            self.connections.load(Ordering::Relaxed),
            articles,
            bytes,
            self.stats_cmds.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.missing.load(Ordering::Relaxed),
            elapsed_secs,
            per_s,
            mibps,
        )
    }
}

/// Cheap deterministic pseudo-randomness for `--drop-pct` / `--miss-pct`.
/// A real PRNG would be overkill: all this needs is a stream of values that
/// is uniform enough to hit a target percentage over thousands of articles.
fn roll(counter: &AtomicU64) -> u32 {
    let n = counter.fetch_add(1, Ordering::Relaxed);
    let mut x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 29;
    (x % 100) as u32
}

fn parse_args() -> Opts {
    let mut opts = Opts::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |i: usize| args.get(i + 1).cloned().unwrap_or_default();
    let ms = |i: usize| -> Option<Duration> {
        let n: u64 = value(i).parse().unwrap_or(0);
        (n > 0).then(|| Duration::from_millis(n))
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                opts.port = value(i).parse().unwrap_or(0);
                i += 1;
            }
            "--latency-ms" => {
                opts.latency = ms(i);
                i += 1;
            }
            "--post-latency-ms" => {
                opts.post_latency = ms(i);
                i += 1;
            }
            "--stats-file" => {
                opts.stats_file = Some(PathBuf::from(value(i)));
                i += 1;
            }
            "--save-dir" => {
                opts.save_dir = Some(PathBuf::from(value(i)));
                i += 1;
            }
            "--drop-pct" => {
                opts.drop_pct = value(i).parse().unwrap_or(0);
                i += 1;
            }
            "--miss-pct" => {
                opts.miss_pct = value(i).parse().unwrap_or(0);
                i += 1;
            }
            "--quiet" => opts.quiet = true,
            // Legacy positional form kept working: `mock_nntp_server 11119`.
            other => {
                if let Ok(p) = other.parse::<u16>() {
                    opts.port = p;
                }
            }
        }
        i += 1;
    }
    opts
}

async fn respond(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    msg: &[u8],
    delay: Option<Duration>,
) -> bool {
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    write.write_all(msg).await.is_ok()
}

async fn handle_connection(stream: TcpStream, opts: Arc<Opts>, stats: Arc<Stats>) {
    // Nagle would coalesce the small status replies with whatever follows and
    // add milliseconds of artificial latency to every command round-trip —
    // exactly the thing this server exists to keep out of the measurement.
    let _ = stream.set_nodelay(true);
    stats.connections.fetch_add(1, Ordering::Relaxed);

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(READ_BUF, read_half);
    if !respond(&mut write_half, b"200 mock nntp ready\r\n", opts.latency).await {
        return;
    }

    let mut line = String::new();
    let mut body: Vec<u8> = Vec::with_capacity(1024 * 1024);
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end();
        // RFC 3977 §3.1: command names are case-insensitive. Only the verb is
        // uppercased; arguments (message-ids, credentials) are left alone.
        let verb: String = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let rest = command[verb.len().min(command.len())..].trim_start();

        match verb.as_str() {
            "AUTHINFO" => {
                let reply: &[u8] = if rest.to_ascii_uppercase().starts_with("USER") {
                    b"381 password required\r\n"
                } else {
                    b"281 authenticated\r\n"
                };
                if !respond(&mut write_half, reply, opts.latency).await {
                    return;
                }
            }
            "POST" | "IHAVE" => {
                if !respond(&mut write_half, b"340 send article\r\n", opts.latency).await {
                    return;
                }
                body.clear();
                if !drain_article(&mut reader, &mut body, opts.save_dir.is_some()).await {
                    return;
                }
                let len = body.len() as u64;

                let reject = opts.drop_pct > 0 && roll(&stats.rejected) < opts.drop_pct;
                if reject {
                    // 441 is what a real server answers on a rejected post; a
                    // poster is expected to retry it.
                    if !respond(&mut write_half, b"441 posting failed\r\n", opts.latency).await {
                        return;
                    }
                    continue;
                }

                if let Some(dir) = &opts.save_dir {
                    save_article(dir, &body, &stats);
                }
                stats.articles.fetch_add(1, Ordering::Relaxed);
                stats.article_bytes.fetch_add(len, Ordering::Relaxed);

                let delay = opts.post_latency.or(opts.latency);
                if !respond(&mut write_half, b"240 article received\r\n", delay).await {
                    return;
                }
            }
            // The streaming check queue's STAT. Real NNTP STAT takes the
            // message-id as an argument (`STAT <id>`), so this matches the
            // verb, not the whole line.
            "STAT" => {
                stats.stats_cmds.fetch_add(1, Ordering::Relaxed);
                let miss = opts.miss_pct > 0 && roll(&stats.missing) < opts.miss_pct;
                let reply: &[u8] = if miss {
                    b"430 no such article\r\n"
                } else {
                    b"223 0 <fake@mock> article exists\r\n"
                };
                if !respond(&mut write_half, reply, opts.latency).await {
                    return;
                }
            }
            "MODE" => {
                if !respond(&mut write_half, b"200 posting allowed\r\n", opts.latency).await {
                    return;
                }
            }
            "GROUP" => {
                if !respond(
                    &mut write_half,
                    b"211 0 0 0 alt.binaries.test\r\n",
                    opts.latency,
                )
                .await
                {
                    return;
                }
            }
            "DATE" => {
                if !respond(&mut write_half, b"111 20260101000000\r\n", opts.latency).await {
                    return;
                }
            }
            "CAPABILITIES" => {
                let reply =
                    b"101 Capability list:\r\nVERSION 2\r\nMODE-READER\r\nPOST\r\nIHAVE\r\n.\r\n";
                if !respond(&mut write_half, reply, opts.latency).await {
                    return;
                }
            }
            "QUIT" => {
                let _ = respond(&mut write_half, b"205 bye\r\n", opts.latency).await;
                return;
            }
            "" => {}
            _ => {
                if !respond(
                    &mut write_half,
                    b"500 command not recognized\r\n",
                    opts.latency,
                )
                .await
                {
                    return;
                }
            }
        }
    }
}

/// The RFC 3977 §3.1.1 multi-line block terminator: a `.` alone on a line.
const TERMINATOR: &[u8; 5] = b"\r\n.\r\n";

/// Read an article body up to and including its `.\r\n` terminator.
///
/// Scans the buffered reader's own chunks instead of reading line by line: a
/// 768 KB article is ~6 000 yEnc lines, and a `read_line` per line puts a
/// per-line loop (and allocation) between the poster and the ACK it is
/// blocked on — which would make this server, not the poster under test, the
/// benchmark's bottleneck. The byte-at-a-time match below still sustains
/// several GB/s per connection, orders of magnitude above what one NNTP
/// connection ever carries.
///
/// Returns false if the connection died mid-article.
async fn drain_article<R>(reader: &mut BufReader<R>, body: &mut Vec<u8>, keep: bool) -> bool
where
    R: tokio::io::AsyncRead + Unpin,
{
    // How many bytes of TERMINATOR have matched so far. Carried across buffer
    // fills, so a terminator straddling two `fill_buf` chunks is still seen.
    // Starts at 2 ("\r\n" already matched) because the very first line of the
    // body may itself be the terminating `.`, with no preceding CRLF.
    let mut matched = 2usize;
    loop {
        let available = match reader.fill_buf().await {
            Ok([]) | Err(_) => return false,
            Ok(buf) => buf,
        };

        let mut end = None;
        for (i, &b) in available.iter().enumerate() {
            if b == TERMINATOR[matched] {
                matched += 1;
                if matched == TERMINATOR.len() {
                    end = Some(i + 1);
                    break;
                }
            } else {
                // Restart: a `\r` can only ever begin a fresh match.
                matched = usize::from(b == b'\r');
            }
        }

        let consume = end.unwrap_or(available.len());
        if keep {
            body.extend_from_slice(&available[..consume]);
        }
        reader.consume(consume);
        if end.is_some() {
            return true;
        }
    }
}

fn save_article(dir: &std::path::Path, body: &[u8], stats: &Stats) {
    let n = stats.saved.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("article-{n:08}.txt"));
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(body);
    }
}

#[tokio::main]
async fn main() {
    let opts = Arc::new(parse_args());
    if let Some(dir) = &opts.save_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("cannot create --save-dir {}: {e}", dir.display());
            std::process::exit(1);
        }
    }

    let listener = match TcpListener::bind(("127.0.0.1", opts.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind 127.0.0.1:{} failed: {e}", opts.port);
            std::process::exit(1);
        }
    };
    // First line of stdout, flushed immediately: harnesses started with
    // `--port 0` parse the port back from here.
    println!("listening on {}", listener.local_addr().unwrap());
    let _ = std::io::stdout().flush();

    let stats = Arc::new(Stats::default());
    let started = std::time::Instant::now();

    if !opts.quiet {
        let stats = Arc::clone(&stats);
        tokio::spawn(async move {
            let mut last = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let now = stats.articles.load(Ordering::Relaxed);
                eprintln!(
                    "mock: {now} articles ({} /s), {} MiB",
                    now - last,
                    stats.article_bytes.load(Ordering::Relaxed) / (1024 * 1024)
                );
                last = now;
            }
        });
    }

    {
        let stats = Arc::clone(&stats);
        let opts = Arc::clone(&opts);
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            let json = stats.to_json(started.elapsed().as_secs_f64());
            if let Some(path) = &opts.stats_file {
                let _ = std::fs::write(path, format!("{json}\n"));
            }
            println!("{json}");
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        });
    }

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        tokio::spawn(handle_connection(
            stream,
            Arc::clone(&opts),
            Arc::clone(&stats),
        ));
    }
}
