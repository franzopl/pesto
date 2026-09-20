//! Keyboard event dispatch for the TUI event loop.

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::app::{self, App};
use crate::events::AppEvent;
use crate::tasks;

/// Handle one key press. Returns `true` when the app should quit.
pub(crate) fn handle_key(
    app: &mut App,
    key: KeyEvent,
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> bool {
    match key.code {
        KeyCode::Char('q')
            if !app.log_panel.searching
                && !app.history.searching
                && app.history.nzb_viewer.is_none()
                && !app.config_state.editing
                && !app.watch.editing
                && !app.show_upload_confirm
                && app.prowlarr.search.is_none()
                && app.prowlarr.batch.is_none() =>
        {
            return true;
        }
        KeyCode::Esc
            if !app.log_panel.searching
                && !app.history.searching
                && app.history.nzb_viewer.is_none()
                && !app.config_state.editing
                && !app.watch.editing
                && !app.show_upload_confirm
                && app.prowlarr.search.is_none()
                && app.prowlarr.batch.is_none() =>
        {
            return true;
        }
        KeyCode::Tab => {
            app.next_tab();
            if app.state == app::AppState::History {
                app.refresh_history();
            }
        }
        KeyCode::BackTab => {
            app.prev_tab();
            if app.state == app::AppState::History {
                app.refresh_history();
            }
        }
        // F1–F7: direct tab jump
        KeyCode::F(1) => {
            app.state = app::AppState::Dashboard;
        }
        KeyCode::F(2) => {
            app.state = app::AppState::Queue;
        }
        KeyCode::F(3) => {
            app.state = app::AppState::Browser;
        }
        KeyCode::F(4) => {
            app.state = app::AppState::History;
            app.refresh_history();
        }
        KeyCode::F(5) => {
            app.state = app::AppState::NzbVault;
            app.load_vault();
        }
        KeyCode::F(6) => {
            app.state = app::AppState::Config;
        }
        KeyCode::F(7) => {
            app.state = app::AppState::Watch;
        }
        // ── Upload config panel (text-edit mode takes priority) ──
        _ if app.show_upload_confirm && app.confirm_editing => match key.code {
            KeyCode::Esc => app.confirm_cancel_edit(),
            KeyCode::Enter => app.confirm_confirm_edit(),
            KeyCode::Backspace => {
                app.confirm_edit_buf.pop();
            }
            KeyCode::Tab => app.confirm_toggle_password_reveal(),
            KeyCode::Char(c) => app.confirm_edit_buf.push(c),
            _ => {}
        },
        // ── Upload config panel (navigation mode) ─────────────
        _ if app.show_upload_confirm => match key.code {
            // y or Ctrl+Enter = start upload
            KeyCode::Char('y') => {
                app.save_upload_prefs();
                app.confirm_close();
                app.state = app::AppState::Dashboard;
                tasks::upload::handle_upload_trigger(app, tx.clone());
            }
            // Esc/n = cancel panel (stay in browser)
            KeyCode::Esc | KeyCode::Char('n') => {
                app.confirm_close();
                app.status_bar.set("Upload cancelled");
            }
            KeyCode::Down | KeyCode::Char('j') => app.confirm_field_next(),
            KeyCode::Up | KeyCode::Char('k') => app.confirm_field_prev(),
            // Enter or e: cycle enum/bool, or enter edit mode for text fields
            KeyCode::Enter | KeyCode::Char('e') => app.confirm_field_activate(),
            // Right/l/Space: advance cycle/number/toggle fields
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                app.confirm_field_increment();
            }
            // Left/h: step cycle/number/toggle fields backwards
            KeyCode::Left | KeyCode::Char('h') => app.confirm_field_decrement(),
            _ => {}
        },
        // ── Prowlarr search overlay (takes priority over all screens) ──
        _ if app.prowlarr.search.is_some() => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.prowlarr.search = None;
                app.status_bar.set("Search closed");
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut s) = app.prowlarr.search {
                    s.move_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut s) = app.prowlarr.search {
                    s.move_up();
                }
            }
            KeyCode::Char('d') => {
                tasks::prowlarr::trigger_prowlarr_download(app, tx.clone());
            }
            _ => {}
        },
        // ── Hook picker overlay (takes priority over screen keys) ──
        _ if app.hook_picker.is_some() => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Esc first cancels a pending re-send confirmation,
                // then (a second time) closes the picker.
                match app.hook_picker.as_mut().and_then(|p| p.pending_confirm) {
                    Some(_) => {
                        if let Some(ref mut p) = app.hook_picker {
                            p.pending_confirm = None;
                        }
                        app.status_bar.set("Re-send cancelled");
                    }
                    None => {
                        app.hook_picker = None;
                        app.status_bar.set("Hook picker closed");
                    }
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut p) = app.hook_picker {
                    p.move_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut p) = app.hook_picker {
                    p.move_up();
                }
            }
            KeyCode::Enter => {
                // Re-sending a hook that already succeeded for this
                // release asks for confirmation first (one extra Enter).
                let needs_confirm = app
                    .hook_picker
                    .as_ref()
                    .map(|p| {
                        p.pending_confirm != Some(p.selected)
                            && p.selected_hook()
                                .map(|h| p.sent_at(h).is_some())
                                .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if needs_confirm {
                    if let Some(ref mut p) = app.hook_picker {
                        p.pending_confirm = Some(p.selected);
                    }
                    app.status_bar
                        .set("Already sent — press Enter again to re-send, Esc to cancel");
                } else {
                    tasks::hooks::run_selected_hook(app, tx.clone());
                }
            }
            _ => {}
        },
        KeyCode::Char('h') if app.state == app::AppState::Browser => {
            app.file_tree.toggle_hidden();
        }
        _ if app.state == app::AppState::Browser => match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.file_tree.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => app.file_tree.select_next(),
            KeyCode::Char(' ') => {
                // Space is the single selection action: queue/unqueue
                // the item under the cursor (file or folder), then
                // advance. The queue is the one source of truth.
                app.toggle_queue_at_cursor();
            }
            KeyCode::Enter => {
                // Enter is navigation only and never touches the
                // queue: enter a directory, or (on a file) just keep
                // the detail panel focused on it. Use Space to queue.
                if let Some(selected) = app.file_tree.get_selected().cloned() {
                    if selected.is_dir() {
                        app.file_tree.current_dir = selected;
                        app.file_tree.refresh();
                        app.file_tree.selected = 0;
                    } else {
                        app.status_bar.set("Press Space to queue/unqueue this file");
                    }
                }
            }
            KeyCode::Char('b') | KeyCode::Backspace | KeyCode::Left => {
                app.file_tree.go_to_parent();
            }
            KeyCode::Char('u') => {
                // The queue already reflects everything marked with
                // Space, so just open the config panel.
                if app.upload_queue.items.is_empty() {
                    app.status_bar
                        .set("Queue is empty — mark files with Space first");
                } else {
                    app.show_upload_confirm = true;
                }
            }
            KeyCode::Char('n') => {
                // Toggle "show only items without an NZB yet".
                app.file_tree.toggle_filter_unbacked();
                let (_, unbacked, _) = app.file_tree.summary();
                if app.file_tree.filter_unbacked {
                    app.status_bar.set(format!(
                        "Showing {} item(s) that still need uploading (n to show all)",
                        unbacked
                    ));
                } else {
                    app.status_bar.set("Showing all items");
                }
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                tasks::prowlarr::trigger_prowlarr_search(app, tx.clone());
            }
            KeyCode::Char('r') => {
                tasks::hooks::trigger_run_hooks(app);
            }
            _ => {}
        },
        // ── Queue screen keys (the home for queue management) ───
        _ if app.state == app::AppState::Queue => match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.upload_queue.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => app.upload_queue.select_next(),
            KeyCode::Char('u') => {
                if app.upload_queue.items.is_empty() {
                    app.status_bar
                        .set("Queue is empty — go to Browser (F3) and mark files with Space");
                } else if app.upload_in_progress {
                    app.status_bar.set("Upload already running");
                } else {
                    app.show_upload_confirm = true;
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if app.upload_in_progress {
                    app.status_bar.set("Cannot edit the queue during upload");
                } else if let Some(removed) = app.remove_queue_selected() {
                    app.status_bar.set(format!("Removed: {}", removed));
                }
            }
            KeyCode::Char('c') => {
                if app.upload_in_progress {
                    app.status_bar.set("Cannot edit the queue during upload");
                } else {
                    let count = app.clear_queue();
                    app.status_bar
                        .set(format!("Cleared {} items from queue", count));
                }
            }
            KeyCode::Char('J') if !app.upload_in_progress => {
                app.upload_queue.move_selected_down();
                app.save_queue();
            }
            KeyCode::Char('K') if !app.upload_in_progress => {
                app.upload_queue.move_selected_up();
                app.save_queue();
            }
            KeyCode::Char('x') if app.upload_in_progress => {
                app.cancel_upload();
            }
            // Search Prowlarr for every queued release and auto-fetch
            // exact-name matches directly into nzb_dir.
            KeyCode::Char('p') | KeyCode::Char('P') => {
                tasks::prowlarr::trigger_prowlarr_queue_search(app, tx.clone());
            }
            _ => {}
        },
        // ── Log panel search (Dashboard) ───────────────────────
        _ if app.state == app::AppState::Dashboard && app.log_panel.searching => match key.code {
            KeyCode::Esc => app.log_panel.search_clear(),
            KeyCode::Enter => app.log_panel.search_confirm(),
            KeyCode::Backspace => app.log_panel.search_pop(),
            KeyCode::Char(c) => app.log_panel.search_push(c),
            _ => {}
        },
        KeyCode::Char('/') if app.state == app::AppState::Dashboard => {
            app.log_panel.start_search();
        }
        // Log scrolling when on Dashboard
        KeyCode::Up | KeyCode::Char('k') if app.state == app::AppState::Dashboard => {
            app.log_panel.scroll_up(1);
        }
        KeyCode::Down | KeyCode::Char('j') if app.state == app::AppState::Dashboard => {
            app.log_panel.scroll_down(1);
        }
        KeyCode::PageUp if app.state == app::AppState::Dashboard => {
            app.log_panel.scroll_up(10);
        }
        KeyCode::PageDown if app.state == app::AppState::Dashboard => {
            app.log_panel.scroll_down(10);
        }
        KeyCode::Char('g') if app.state == app::AppState::Dashboard => {
            app.log_panel.scroll_to_top();
        }
        KeyCode::Char('G') if app.state == app::AppState::Dashboard => {
            app.log_panel.scroll_to_bottom();
        }
        KeyCode::Char('a') if app.state == app::AppState::Dashboard && !app.log_panel.searching => {
            app.log_panel.toggle_auto_scroll();
        }
        // Cancel current upload (Dashboard shows the live progress)
        KeyCode::Char('x') if app.state == app::AppState::Dashboard && app.upload_in_progress => {
            app.cancel_upload();
        }
        // Pause/resume current upload
        KeyCode::Char('p') if app.state == app::AppState::Dashboard && app.upload_in_progress => {
            app.toggle_pause_upload();
        }
        // ── History screen keys ────────────────────────────────
        // NZB viewer overlay takes priority when open
        _ if app.state == app::AppState::History && app.history.nzb_viewer.is_some() => {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => app.close_nzb_viewer(),
                KeyCode::Char('j') | KeyCode::Down => app.nzb_viewer_scroll_down(),
                KeyCode::Char('k') | KeyCode::Up => app.nzb_viewer_scroll_up(),
                _ => {}
            }
        }
        _ if app.state == app::AppState::History && !app.history.searching => match key.code {
            KeyCode::Char('j') | KeyCode::Down => app.history_select_next(),
            KeyCode::Char('k') | KeyCode::Up => app.history_select_prev(),
            KeyCode::Enter => app.open_nzb_viewer(),
            KeyCode::Char('s') => {
                app.history.show_stats = !app.history.show_stats;
                if app.history.show_stats {
                    app.refresh_stats();
                }
            }
            KeyCode::Char('/') => {
                app.history.searching = true;
            }
            _ => {}
        },
        _ if app.state == app::AppState::History && app.history.searching => match key.code {
            KeyCode::Esc => {
                app.history.searching = false;
                app.history.query.clear();
                app.refresh_history();
            }
            KeyCode::Enter => {
                app.history.searching = false;
                app.refresh_history();
            }
            KeyCode::Backspace => {
                app.history.query.pop();
                app.refresh_history();
            }
            KeyCode::Char(c) => {
                app.history.query.push(c);
                app.refresh_history();
            }
            _ => {}
        },
        // ── Config screen keys ────────────────────────────────
        // ── NZB Vault viewer overlay ─────────────────────────────
        _ if app.state == app::AppState::NzbVault && app.vault.viewer.is_some() => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.vault.viewer = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut v) = app.vault.viewer {
                    v.scroll = v.scroll.saturating_add(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut v) = app.vault.viewer {
                    v.scroll = v.scroll.saturating_sub(1);
                }
            }
            _ => {}
        },
        // ── NZB Vault list ────────────────────────────────────────
        _ if app.state == app::AppState::NzbVault => match key.code {
            KeyCode::Char('j') | KeyCode::Down => app.vault.move_down(),
            KeyCode::Char('k') | KeyCode::Up => app.vault.move_up(),
            KeyCode::Enter => {
                app.vault_parse_selected();
            }
            KeyCode::Char('v') => {
                app.vault_open_viewer();
            }
            KeyCode::Char('s') => {
                app.vault.cycle_sort();
                app.status_bar.set(format!("Sort: {:?}", app.vault.sort));
            }
            KeyCode::Char('r') => {
                app.load_vault();
            }
            KeyCode::Char('d') => {
                if let Some(entry) = app.vault.selected_entry() {
                    let path = entry.path.clone();
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            app.status_bar.set(format!(
                                "Deleted {}",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ));
                            app.load_vault();
                        }
                        Err(e) => {
                            app.status_bar.set(format!("Delete failed: {}", e));
                        }
                    }
                }
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                tasks::prowlarr::trigger_prowlarr_search(app, tx.clone());
            }
            _ => {}
        },
        _ if app.state == app::AppState::Config && app.config_state.editing => match key.code {
            KeyCode::Esc => app.config_cancel_edit(),
            KeyCode::Enter => app.config_confirm_edit(),
            KeyCode::Backspace => {
                app.config_state.edit_buf.pop();
            }
            KeyCode::Char(c) => {
                app.config_state.edit_buf.push(c);
            }
            _ => {}
        },
        _ if app.state == app::AppState::Config => match key.code {
            KeyCode::Char('j') | KeyCode::Down => app.config_select_next(),
            KeyCode::Char('k') | KeyCode::Up => app.config_select_prev(),
            KeyCode::Enter | KeyCode::Char('e') => app.config_start_edit(),
            KeyCode::Char('r') => app.config_reset_field(),
            KeyCode::Char('R') => app.config_reset_all(),
            KeyCode::Char('C') => {
                tasks::prowlarr::trigger_prowlarr_check(app, tx.clone());
            }
            _ => {}
        },
        // ── Watch screen (text-edit mode) ──────────────────────
        _ if app.state == app::AppState::Watch && app.watch.editing => match key.code {
            KeyCode::Esc => app.watch_cancel_edit(),
            KeyCode::Enter => app.watch_confirm_edit(),
            KeyCode::Backspace => {
                app.watch.edit_buf.pop();
            }
            KeyCode::Char(c) => {
                app.watch.edit_buf.push(c);
            }
            _ => {}
        },
        // ── Watch screen (navigation) ──────────────────────────
        _ if app.state == app::AppState::Watch => match key.code {
            KeyCode::Char('j') | KeyCode::Down => app.watch_select_next(),
            KeyCode::Char('k') | KeyCode::Up => app.watch_select_prev(),
            KeyCode::Enter | KeyCode::Char('e') => app.watch_start_edit(),
            KeyCode::Char('w') => app.toggle_watch_enabled(),
            _ => {}
        },
        _ => {}
    }
    false
}
