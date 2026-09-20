use std::path::PathBuf;

use anyhow::{Context, Result};
use penne::check::CheckMethod;

/// `penne check`: first-class article availability checker with JSON output,
/// exit codes (0=all present, 1=confirmed missing, 2=fatal error,
/// 3=inconclusive — no confirmed-missing segment, but at least one
/// unreachable), multi-NZB support, configurable pipeline depth, and quiet
/// mode.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    nzb_paths: &[PathBuf],
    method: CheckMethod,
    sample: Option<usize>,
    pipeline_depth: usize,
    fail_fast: bool,
    json: bool,
    quiet: bool,
    config_path: Option<PathBuf>,
    server_names: &[String],
    independent_servers: bool,
) -> Result<i32> {
    let config_path = match config_path {
        Some(path) => path,
        None => {
            let default = penne::config::default_config_path()
                .context("cannot locate a config directory: set $HOME or $XDG_CONFIG_HOME")?;
            anyhow::ensure!(
                default.exists(),
                "no config found at {}; run `penne --config` to create one, or pass --config <FILE>",
                default.display()
            );
            default
        }
    };
    let config_toml = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = penne::config::RawConfig::parse(&config_toml)?
        .select(server_names)?
        .resolve()?;
    anyhow::ensure!(
        !config.server_tiers.is_empty(),
        "no [[servers]] configured in {}",
        config_path.display()
    );

    let check_config = penne::check::CheckConfig {
        method,
        pipeline_depth,
        retries: config.retries,
        fail_fast,
    };

    let flat_servers: Vec<penne::config::ServerTier> = config
        .server_tiers
        .iter()
        .flat_map(|tier| tier.members.iter().cloned())
        .map(penne::config::ServerTier::solo)
        .collect();

    let mut any_missing = false;
    let mut any_unreachable = false;

    let mut queues = Vec::new();
    let mut nzb_names = Vec::new();
    let mut total_segments = 0;

    for nzb_path in nzb_paths {
        let parsed = penne::nzb::load(nzb_path)?;
        let mut queue = penne::queue::build(&parsed);
        if let Some(per_file) = sample {
            queue = penne::queue::sample(&queue, per_file);
        }
        total_segments += queue.files.iter().map(|f| f.segments.len()).sum::<usize>();
        queues.push(queue);
        nzb_names.push(
            nzb_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string(),
        );
    }

    let tiers_to_run: Vec<Vec<penne::config::ServerTier>> = if independent_servers {
        flat_servers.iter().map(|t| vec![t.clone()]).collect()
    } else {
        vec![config.server_tiers.clone()]
    };

    // Gated on `!quiet` only, not `!json`: the live bar and this banner both
    // write to stderr (see ui/check.rs), never stdout, so they don't corrupt
    // `--json`'s NDJSON output on stdout — a caller piping stdout to a file
    // (e.g. curupira's remote-check.sh) still gets to watch progress on the
    // terminal instead of sitting with zero feedback through a long batch.
    if !quiet {
        if independent_servers {
            eprintln!(
                "checking {} segment(s) across {} NZB(s) via {} on {} server(s) concurrently...",
                total_segments,
                queues.len(),
                method,
                flat_servers.len()
            );
        } else {
            eprintln!(
                "checking {} segment(s) across {} NZB(s) via {}...",
                total_segments,
                queues.len(),
                method
            );
        }
    }

    let total_work = total_segments * tiers_to_run.len();
    let (tx, rx) = penne::check::channel();
    let progress_task = if !quiet {
        Some(penne::ui::check::spawn_renderer(rx, total_work as u32))
    } else {
        drop(rx);
        None
    };

    let (outcome_tx, mut outcome_rx) = tokio::sync::mpsc::unbounded_channel::<(
        Option<String>,
        usize,
        penne::check::CheckOutcome,
    )>();
    let nzb_names_clone = nzb_names.clone();
    let is_json = json;
    let method_str = method.to_string();
    let retries = check_config.retries;
    // Hostnames tried, in priority order, when servers are combined into a
    // single check rather than run `--independent-servers` (which already
    // reports its own server per line via `server_label`).
    let aggregated_servers: Vec<String> = config
        .server_tiers
        .iter()
        .flat_map(|tier| tier.members.iter().map(|m| m.host.clone()))
        .collect();

    let print_task = tokio::spawn(async move {
        if is_json {
            while let Some((server_label, q_idx, outcome)) = outcome_rx.recv().await {
                let nzb_name = &nzb_names_clone[q_idx];
                let mut json_val = serde_json::json!({
                    "nzb": nzb_name,
                    // Wall-clock time this outcome was resolved, not when the
                    // check started — with multiple NZBs/servers finishing at
                    // different times, a single run-start timestamp would be
                    // misleading for the later lines.
                    "checked_at": chrono::Utc::now().to_rfc3339(),
                    "method": method_str,
                    "retries": retries,
                    // `complete` requires every segment be confirmed present —
                    // false if any is confirmed missing OR merely unreachable.
                    // Check `conclusive` before trusting `missing`/`missing_pct`
                    // as a final verdict: if `conclusive` is false, at least one
                    // segment never got a real answer from any server, and
                    // treating that as confirmed absence is the false positive
                    // this field split exists to prevent.
                    "complete": outcome.is_complete(),
                    "conclusive": outcome.is_conclusive(),
                    "stopped_early": outcome.stopped_early,
                    "total_articles": outcome.total_checked,
                    "present": outcome.total_present,
                    "missing": outcome.missing_count(),
                    "missing_pct": if outcome.total_checked > 0 {
                        outcome.missing_count() as f64 / outcome.total_checked as f64 * 100.0
                    } else {
                        0.0
                    },
                    "unreachable": outcome.unreachable_count(),
                    "skipped": outcome.skipped,
                    "unreachable_pct": if outcome.total_checked > 0 {
                        outcome.unreachable_count() as f64 / outcome.total_checked as f64 * 100.0
                    } else {
                        0.0
                    },
                    "files": outcome.files,
                    "missing_articles": outcome.missing,
                    "unreachable_articles": outcome.unreachable,
                    "bytes_used": outcome.bytes_used,
                    "elapsed_secs": outcome.elapsed.as_secs_f64(),
                    "articles_per_second": outcome.articles_per_second(),
                });
                let obj = json_val.as_object_mut().unwrap();
                if let Some(ref s) = server_label {
                    obj.insert("server".to_string(), serde_json::Value::String(s.clone()));
                } else {
                    obj.insert(
                        "servers".to_string(),
                        serde_json::to_value(&aggregated_servers).unwrap(),
                    );
                }
                if let Some(n) = sample {
                    obj.insert("sample_size".to_string(), serde_json::Value::from(n));
                }
                println!("{}", serde_json::to_string(&json_val).unwrap());
            }
        } else {
            // Drain the channel so it doesn't block senders
            while outcome_rx.recv().await.is_some() {}
        }
    });

    let mut join_set = tokio::task::JoinSet::new();

    for tiers_batch in tiers_to_run {
        let server_label = if independent_servers {
            let s = &tiers_batch[0].members[0];
            Some(s.host.clone())
        } else {
            None
        };

        let qs = queues.clone();
        let tb = tiers_batch.clone();
        let c = check_config.clone();
        let txc = tx.clone();

        let out_tx = outcome_tx.clone();
        join_set.spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

            let forward_task = {
                let sl = server_label.clone();
                tokio::spawn(async move {
                    while let Some((q_idx, outcome)) = rx.recv().await {
                        let _ = out_tx.send((sl.clone(), q_idx, outcome));
                    }
                })
            };

            let outcomes = penne::check::check_nzbs(&qs, &tb, &c, Some(txc), Some(tx)).await;
            forward_task.await.ok();
            (server_label, outcomes)
        });
    }

    drop(tx);
    drop(outcome_tx);

    let mut tier_outcomes = Vec::new();
    while let Some(res) = join_set.join_next().await {
        tier_outcomes.push(res.expect("task panicked"));
    }

    if let Some(task) = progress_task {
        task.await.ok();
    }
    print_task.await.ok();

    tier_outcomes.sort_by(|a, b| a.0.cmp(&b.0));

    for (server_label, outcomes_res) in tier_outcomes {
        let outcomes = outcomes_res?;
        for (i, outcome) in outcomes.into_iter().enumerate() {
            let nzb_name = &nzb_names[i];

            if !json {
                if queues.len() > 1 {
                    println!("\n[{}/{}] {}", i + 1, queues.len(), nzb_name);
                }

                let incomplete_files =
                    outcome.files.iter().filter(|f| !f.is_complete()).count() as u32;
                if !quiet {
                    for f in &outcome.files {
                        if f.is_complete() {
                            println!(
                                "  complete: {} ({}/{} segments)",
                                f.name, f.present_segments, f.total_segments
                            );
                        } else {
                            println!(
                                "  INCOMPLETE: {} ({}/{} segments)",
                                f.name, f.present_segments, f.total_segments
                            );
                        }
                    }
                    for seg in &outcome.missing {
                        println!("    missing: {} part {}", seg.file_name, seg.part);
                    }
                    for seg in &outcome.unreachable {
                        println!(
                            "    unreachable: {} part {} (no server gave a definitive answer)",
                            seg.file_name, seg.part
                        );
                    }
                }

                let present_pct = if outcome.total_checked > 0 {
                    outcome.total_present as f64 / outcome.total_checked as f64 * 100.0
                } else {
                    100.0
                };
                let complete_files = outcome.files.len() as u32 - incomplete_files;

                println!();
                if let Some(ref s) = server_label {
                    println!("summary ({s})");
                } else {
                    println!("summary");
                }
                println!(
                    "  articles present: {}/{} ({:.1}%)",
                    outcome.total_present, outcome.total_checked, present_pct
                );
                if !outcome.unreachable.is_empty() {
                    let unreachable_pct = outcome.unreachable.len() as f64
                        / outcome.total_checked.max(1) as f64
                        * 100.0;
                    println!(
                        "  unreachable:       {} ({:.1}%) — no server gave a definitive answer; \
                         not counted as missing, but this check is inconclusive",
                        outcome.unreachable.len(),
                        unreachable_pct
                    );
                }
                if fail_fast && !outcome.missing.is_empty() {
                    println!("  result:           INCOMPLETE — confirmed missing article");
                    if outcome.stopped_early {
                        println!(
                            "  stopped early:    {} segment(s) not checked",
                            outcome.skipped
                        );
                    }
                    if let Some(seg) = outcome.missing.first() {
                        println!("  first missing:    {} part {}", seg.file_name, seg.part);
                    }
                }
                println!(
                    "  files complete:   {}/{}",
                    complete_files,
                    outcome.files.len()
                );
                let data_used_note = match method {
                    CheckMethod::Stat => "STAT only — no article data downloaded",
                    CheckMethod::Head => "HEAD only — headers only, no article body downloaded",
                    CheckMethod::Body => {
                        "full BODY fetch — real article data downloaded, nothing written to disk"
                    }
                };
                println!(
                    "  data used:        {} ({data_used_note})",
                    pesto::progress::format_size(outcome.bytes_used)
                );
                println!(
                    "  elapsed:          {:.1}s ({:.0} articles/sec)",
                    outcome.elapsed.as_secs_f64(),
                    outcome.articles_per_second()
                );
            }

            if !outcome.missing.is_empty() {
                any_missing = true;
            }
            if !outcome.unreachable.is_empty() {
                any_unreachable = true;
            }
        }
    }

    // A confirmed-missing article always wins over "merely inconclusive" —
    // once we know for certain the release is broken, that's more
    // actionable than "we couldn't fully confirm it".
    Ok(if any_missing {
        1
    } else if any_unreachable {
        3
    } else {
        0
    })
}
