//! Conversion of parsed command-line flags to configuration overrides.

use pesto::config::{parse_memory_limit_spec, parse_upload_rate, Overrides};

use super::Cli;

impl Cli {
    /// Build config [`Overrides`] from the parsed flags.
    pub(crate) fn overrides(&self) -> Overrides {
        Overrides {
            host: self.host.clone(),
            port: self.port,
            // `--no-ssl` is the only TLS flag; absent means "defer to config".
            proxy: self.proxy.clone(),
            proxy_check_ip: if self.proxy_check_ip {
                Some(true)
            } else {
                None
            },
            ssl: if self.no_ssl { Some(false) } else { None },
            connections: self.connections,
            username: self.username.clone(),
            password: self.password.clone(),
            from: self.from.clone(),
            groups: if self.groups.is_empty() {
                None
            } else {
                Some(self.groups.clone())
            },
            article_size: self.article_size,
            line_length: self.line_length,
            retries: self.retries,
            retry_delay: self.retry_delay,
            obfuscate: self.obfuscate,
            dry_run: if self.dry_run { Some(true) } else { None },
            par2: self.par2,
            par2_only: if self.par2_only { Some(true) } else { None },
            par2_before_upload: if self.par2_before_upload {
                Some(true)
            } else {
                None
            },
            par2_memory_limit: self
                .par2_memory_limit
                .as_ref()
                .and_then(|s| parse_upload_rate(s).ok()),
            memory_limit: self
                .memory_limit
                .as_ref()
                .and_then(|s| parse_memory_limit_spec(s).ok().flatten()),
            par2_temp_dir: self.par2_temp_dir.clone(),
            par2_slice_size: self.slice_size,
            par2_effort: self.par2_effort,
            par2_slice_count: self.slice_count,
            par2_recovery_count: self.recovery_count,
            threads: self.threads,
            simd: self.simd,
            resume: if self.resume { Some(true) } else { None },
            upload_rate: self
                .rate
                .as_deref()
                .map(parse_upload_rate)
                .transpose()
                .unwrap_or(None),
            compress_format: self.compress.clone(),
            compress_temp_dir: self.compress_temp_dir.clone(),
            compress_volume_size: self.compress_volume_size.clone(),
            // None → no password (flag absent, or bare `--password` for
            // auto-random). Some(s) → an explicit password, reused verbatim
            // by every entry under --each/--season/--watch. The bare-flag
            // case is deliberately *not* resolved here: doing so used to
            // bake one random password into `Config` for the whole process,
            // so every entry under --each/--watch silently shared it
            // instead of getting its own (issue #67). It's resolved lazily
            // per upload instead — see `run_single_upload`'s
            // `effective_password` and `run_batch`'s `season_password`.
            compress_password: self
                .archive_password
                .as_deref()
                .and_then(|pw| (!pw.is_empty()).then(|| pw.to_string())),
            nzb_title: self.nzb_title.clone().or_else(|| {
                self.nzb_name.clone().inspect(|_| {
                    eprintln!(
                        "warning: --nzb-name is deprecated, use --nzb-title instead; \
                         --nzb-name will stop being accepted in a future release"
                    );
                })
            }),
            nzb_password: self.nzb_password.clone(),
            nzb_category: self.nzb_category.clone(),
            nzb_tags: self.nzb_tag.clone(),
            tmdb: self.tmdb.clone(),
            imdb_id: self.imdb_id.clone(),
            tvdb_id: self.tvdb_id.clone(),
            mal_id: self.mal_id.clone(),
            nzb_dir: self
                .nzb_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            history: if self.no_history { Some(false) } else { None },
            notify: if self.no_notify {
                Some(false)
            } else if self.notify {
                Some(true)
            } else {
                None
            },
            date: self.date.clone(),
            no_archive: if self.no_archive { Some(true) } else { None },
            file_counter: if self.no_file_counter {
                Some(false)
            } else if self.file_counter {
                Some(true)
            } else {
                None
            },
            message_id_domain: self.message_id_domain.clone(),
            pre_hooks: self.pre_hook.clone(),
            post_hooks: self.post_hook.clone(),
            ext: if self.ext.is_empty() {
                None
            } else {
                Some(self.ext.clone())
            },
            exclude: if self.exclude.is_empty() {
                None
            } else {
                Some(self.exclude.clone())
            },
            no_exclude: if self.no_exclude { Some(true) } else { None },
            no_hooks: if self.no_hooks { Some(true) } else { None },
            nfo: if self.nfo { Some(true) } else { None },
            nzb_conflict: if self.no_overwrite {
                Some(pesto::config::NzbConflict::Rename)
            } else {
                self.nzb_conflict
            },
            check: if self.no_check {
                Some(false)
            } else if self.check {
                Some(true)
            } else {
                None
            },
            check_delay_secs: self.check_delay,
            check_retries: self.check_retries,
            check_connections: self.check_connections,
            check_post_retries: self.check_post_retries,
            allow_incomplete_nzb: if self.allow_incomplete_nzb {
                Some(true)
            } else {
                None
            },
            check_recover_percent: self.check_recover_percent,
            check_recover_max: self.check_recover_max,
            pipeline_depth: self.pipeline_depth,
        }
    }
}
