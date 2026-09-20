//! Hook picker and hook execution tasks.

use std::path::PathBuf;

use pesto::config::ObfuscateMode;
use tokio::sync::mpsc;

use crate::app::{self, App};
use crate::events::AppEvent;
use crate::{prowlarr, ui};

/// Called when the user presses 'r' on the Browser (or Vault): open the hook
/// picker so the user runs one chosen hook against the selected release.
///
/// Resolves the selected item to a release name (and a direct `.nzb` path when
/// the selection already is an NZB), lists the executable scripts in
/// `~/.config/pesto/hooks/`, and shows the picker. The actual run happens in
/// [`run_selected_hook`] once the user confirms.
pub(crate) fn trigger_run_hooks(app: &mut App) {
    if app.pesto_config.is_none() {
        app.status_bar
            .set("pesto.toml not loaded — needed to locate nzb_dir and hooks");
        return;
    }

    // Resolve the selected item to a release name and, when possible, a direct
    // `.nzb` path (Vault entries and `.nzb` files are already NZBs) plus the
    // media path on disk (so a `.nfo` can be generated when none exists yet).
    let (release_name, direct_nzb, media_path): (String, Option<PathBuf>, Option<PathBuf>) =
        match app.state {
            app::AppState::NzbVault => match app.vault.selected_entry() {
                Some(e) => (
                    prowlarr::release_name_from_filename(&e.name).to_string(),
                    Some(e.path.clone()),
                    None,
                ),
                None => {
                    app.status_bar.set("Nothing selected to run hooks on");
                    return;
                }
            },
            app::AppState::Browser => match app.file_tree.get_selected().cloned() {
                Some(p) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let is_nzb = p
                        .extension()
                        .map(|x| x.eq_ignore_ascii_case("nzb"))
                        .unwrap_or(false);
                    // A release folder has no extension — stripping one would drop
                    // a group tag like `.DUAL-Kallango`, breaking the release-key
                    // match. Only files carry a container/.nzb extension to strip.
                    let release_name = if p.is_dir() {
                        name.clone()
                    } else {
                        prowlarr::release_name_from_filename(&name).to_string()
                    };
                    // The media path is the selection itself, unless it already
                    // is the `.nzb` (then there's no media to mediainfo).
                    let media_path = (!is_nzb).then(|| p.clone());
                    (release_name, is_nzb.then_some(p), media_path)
                }
                None => {
                    app.status_bar.set("Nothing selected to run hooks on");
                    return;
                }
            },
            _ => {
                app.status_bar.set("Select a release in Browser or Vault");
                return;
            }
        };

    let hooks = pesto::hooks::list_hook_scripts();
    if hooks.is_empty() {
        app.status_bar
            .set("No executable hooks in ~/.config/pesto/hooks/");
        return;
    }

    // Past successful runs for this release, so the picker can flag what was
    // already sent and confirm before re-sending.
    let runs = {
        let key = ui::components::file_tree::release_key(&release_name);
        app.catalog
            .as_ref()
            .and_then(|c| c.hook_runs_for(&key).ok())
            .unwrap_or_default()
    };

    app.hook_picker = Some(app::HookPickerState::new(
        release_name,
        direct_nzb,
        media_path,
        hooks,
        runs,
    ));
}

/// Run the hook chosen in the picker against the selected release.
///
/// Closes the overlay, resolves the `.nzb` (a directly selected one wins;
/// otherwise the matching release in `nzb_dir` by release key), finds a sibling
/// `.nfo`, and runs exactly that one hook with the usual `PESTO_*` environment.
/// Runs off-thread; output streams back via [`AppEvent::HooksDone`].
pub(crate) fn run_selected_hook(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    let Some(picker) = app.hook_picker.take() else {
        return;
    };
    let Some(hook) = picker.selected_hook().cloned() else {
        app.status_bar.set("No hook selected");
        return;
    };
    let Some(cfg) = app.pesto_config.clone() else {
        app.status_bar.set("pesto.toml not loaded");
        return;
    };

    let nzb_dir = cfg.nzb_dir.as_deref().map(app::expand_tilde);
    let release_name = picker.release_name;
    let direct_nzb = picker.direct_nzb;
    let media_path = picker.media_path;
    let hook_name = hook
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| hook.display().to_string());

    app.status_bar
        .set(format!("Running hook {hook_name} for \"{release_name}\"…"));
    app.log_panel.push(format!(
        "=== Running hook {hook_name} for {release_name} ==="
    ));

    tokio::task::spawn_blocking(move || {
        // Locate the NZB: a directly selected one wins; otherwise search nzb_dir
        // by release key (so a media folder maps to its release/season .nzb).
        let nzb_path = direct_nzb.or_else(|| {
            nzb_dir
                .as_deref()
                .and_then(|d| app::find_nzb_for_release(d, &release_name))
        });
        let Some(nzb_path) = nzb_path else {
            let _ = tx.send(AppEvent::HooksDone {
                ok: false,
                release_key: String::new(),
                release_name: release_name.clone(),
                hook_name: hook_name.clone(),
                log: vec![format!("No .nzb found for \"{release_name}\" in nzb_dir")],
            });
            return;
        };

        // Resolve a `.nfo`: an existing sibling wins; otherwise generate one from
        // the local media via mediainfo and persist it next to the `.nzb` so the
        // hook (e.g. Curupira) gets PESTO_NFO. Best-effort — a missing mediainfo
        // or non-media selection just leaves PESTO_NFO empty as before.
        let mut nfo_log: Option<String> = None;
        let nfo_path = app::find_sibling_nfo(&nzb_path).or_else(|| {
            let media = media_path.as_ref()?;
            let content = pesto::nfo::generate(std::slice::from_ref(media))?;
            let dest = nzb_path.with_extension("nfo");
            match pesto::nfo::write(&dest, &content) {
                Ok(()) => {
                    nfo_log = Some(format!("generated .nfo via mediainfo: {}", dest.display()));
                    Some(dest)
                }
                Err(e) => {
                    nfo_log = Some(format!("could not write .nfo: {e}"));
                    None
                }
            }
        });
        let total_bytes = std::fs::metadata(&nzb_path).map(|m| m.len()).unwrap_or(0);

        // No live PostOutcome here (this re-runs hooks for an already
        // completed NZB), so report every configured server rather than
        // just the primary — mirrors the pre-hook case in the `pesto` CLI.
        let servers_str = cfg
            .all_servers()
            .map(|s| s.host)
            .collect::<Vec<_>>()
            .join(":");
        let ctx = pesto::hooks::HookContext {
            name: release_name.clone(),
            total_bytes,
            input_paths: String::new(),
            server: servers_str
                .split(':')
                .next()
                .unwrap_or(&cfg.host)
                .to_string(),
            servers: servers_str,
            group: cfg.groups.first().cloned().unwrap_or_default(),
            groups: cfg.groups.join(":"),
            password: cfg
                .nzb_password
                .as_deref()
                .or(cfg.compress_password.as_deref())
                .unwrap_or("")
                .to_string(),
            category: cfg.nzb_category.clone().unwrap_or_default(),
            nzb_title: cfg.nzb_title.clone().unwrap_or_default(),
            obfuscate: match cfg.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::Light => "light",
                ObfuscateMode::FullShared => "full-shared",

                ObfuscateMode::Article => "article",
            }
            .to_string(),
            par2: cfg.par2,
            tags: cfg.nzb_tags.join(" "),
            nzb_path: nzb_path.to_string_lossy().into_owned(),
            nfo_path: nfo_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            // No live `PostedSegment`s here (this re-runs hooks for an
            // already-completed NZB) — the `.nzb` itself only ever carries
            // the real filename (see `nzb::generate`'s doc comment), never
            // the wire identity, so there is nothing to recover it from.
            wire_subject: String::new(),
            incomplete: false,
        };

        let (ok, mut log) = pesto::hooks::run_one_hook(&hook, &ctx);
        if let Some(line) = nfo_log {
            log.insert(0, line);
        }
        let release_key = ui::components::file_tree::release_key(&release_name);
        let _ = tx.send(AppEvent::HooksDone {
            ok,
            release_key,
            release_name,
            hook_name,
            log,
        });
    });
}
