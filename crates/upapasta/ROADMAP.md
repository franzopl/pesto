# UpaPasta v2 — Roadmap

> **Scope:** This document governs the `upapasta` crate only.
> `pesto` (core library) and `parmesan` (PAR2 engine) have separate concerns.
>
> UpaPasta is a **NZB backup manager**. It uploads files to Usenet and keeps
> a catalogue of the resulting NZBs. It does **not** download Usenet content.

---

## Philosophy

- Every screen must be useful at a glance without reading documentation.
- Status must be immediately visible: uploaded, pending, missing, broken.
- Upload settings are chosen per-batch, not per-application-launch.
- Progress during upload must be honest and detailed — the user is watching.
- Prowlarr/indexer integration is passive: search, verify, archive. Never download media.

---

## Phase 40c — Usability Reset (CRITICAL — blocks everything else)

> **Why this exists.** The tool currently passes its own feature checklist but
> still fails the one job a user actually wants: *"browse my files, pick the
> ones I want to back up, queue several of them, and let pesto upload them one
> after another — one NZB per release."* Real usage exposed gaps that no
> "✅ Done" milestone caught. This phase is about making the happy path
> trustworthy before adding more screens.

### The user's mental model (what we must honor)

1. I open the file manager and see my disk.
2. I see at a glance **what still needs uploading** (no NZB yet).
3. I mark several items — files *and* folders.
4. I press one key. Pesto uploads them **in sequence**.
5. Each item becomes **its own NZB**, named after the item. A folder/release
   becomes **one** NZB, not one per inner file, and never all items merged.
6. When it finishes, the catalog and the NZB vault reflect exactly what happened
   — including partial failures.

### 40c-1 — One coherent selection model ✅ Done

> Implemented. The upload queue is now the single source of truth. `Space`
> queues/unqueues the item under the cursor (file *or* folder) and advances;
> `Enter` is navigation only and never mutates the queue; `u` opens the config
> panel for whatever is queued. The Browser `[x]` badge is a render mirror of the
> queue (`FileTree::set_queued`), so the file list and the queue panel can never
> disagree. The old `toggle_mark`/`take_marked` second selection set was removed.



**Problem:** there are two competing selection systems in the Browser.
`Space` marks into a browser-local set; `Enter` *also* toggles the item
directly into the queue; `u` then moves marked items into the queue. Three
verbs, two states, one confused user. The keybinding map at the bottom of this
document (Space = mark, Enter = enter dir / open detail) does **not** match the
code.

**Target:**
- `Space` — the *only* way to mark/unmark an item (file or directory) for the
  queue. Marking is the queue. No hidden second set.
- `Enter` — navigation only: enter a directory, or open the detail/NZB panel for
  a file. Never mutates the queue.
- `u` — review marked items in the upload-config panel, then confirm.
- Marked state must be visibly identical in the Browser and in the Queue view
  (same items, same order, one source of truth).

### 40c-2 — Folder = one release = one NZB ✅ Done

> Implemented. Marking a directory queues it as a single entry; `pesto`'s
> `expand_inputs` already walks it into one NZB named after the folder (folder
> names keep their dots — they are not extensions). The queue panel, the upload
> config panel, and the NZB-status detail now show, per entry, the resulting
> `<name>.nzb` and, for folders, the bundled file count (`📁 Movie (2024) →
> 1 NZB (3 files)`), so there is never a surprise before confirming. Counts are
> cached per queue entry (`App::queue_meta`) to keep the render loop off the
> filesystem.



**Problem:** marking is file-by-file. A movie folder (`movie.mkv` + `sample` +
`.nfo`) cannot be queued as a single release. And the old "merge everything into
one NZB" bug (fixed in `28caebc`, one NZB per queue item) was the *opposite*
overcorrection — neither extreme is what users want.

**Target — explicit, predictable grouping:**
- Marking a **file** ⇒ one NZB for that file.
- Marking a **directory** ⇒ one NZB containing every file under it, named after
  the directory. This is the normal Usenet "release" unit.
- The upload-confirm panel must show, per queue entry: the resulting **NZB name**
  and how many files it bundles, so there is never a surprise.
- Never silently merge unrelated queue entries into a single NZB.

### 40c-3 — Honest sequential queue + honest catalog ✅ Done

> Implemented. Catalog records are now written **per item as it finishes**
> (`ItemUploadDone` → `App::item_upload_done`) using the **real** byte size and
> the **actual** NZB path from `pesto`'s `UploadOutcome` — the fabricated
> `total / count` average is gone. A failure no longer erases the batch: each
> success is already committed, failed items stay in the queue marked `✗` for
> retry (press `u` again), and successful items leave the queue and gain the
> uploaded `✓` badge. The queue and the progress screen show live per-item
> state (`○ queued / ▶ uploading / ✓ done / ✗ failed`) from `App::queue_status`,
> and the Browser NZB column refreshes as each item lands.



**Problem:** uploads already run sequentially (good), but bookkeeping is done
*once at the end* using a global success flag:
- `upload_finished` records the catalog only `if success && !cancelled`, so if
  item 2 of 3 fails, **none** of the three are recorded — even the ones that
  uploaded fine.
- Per-file size is faked: `size_each = total_bytes / item_count`. Every catalog
  record gets the batch average, not its real size.

**Target:**
- Record each item in the catalog **as it finishes**, with its **real** byte
  size and the **actual** NZB path that pesto wrote.
- A failed item marks only itself as failed and leaves a visible retry affordance
  in the queue; it does not erase the successes.
- The queue view shows live per-item state: `queued / uploading / done / failed`,
  matching the NZB-status column in the Browser.

### 40c-4 — "What should I upload?" at a glance ✅ Done

> Implemented. `n` toggles an **unbacked-only** filter in the Browser, hiding
> everything already in the catalog so only what still needs uploading remains
> (the border turns magenta and the title shows `• filter:unbacked`). A
> directory counts as needing upload when it was not uploaded as a release and
> any file under it is still uncatalogued (`dir_has_unbacked`, capped walk). The
> Browser title carries a live summary — `N items · M unbacked · X GB to upload`
> (or `all backed ✓`) — recomputed on navigation and whenever the catalog
> changes, so per-item uploads update it immediately.



**Problem:** the NZB-status column exists, but there is no way to *filter* the
Browser down to "things I have not backed up yet." The user's first question is
always "what still needs uploading?" and today they must eyeball every row.

**Target:**
- A toggle (e.g. `n`) to filter the Browser to items with **no NZB** (not in
  catalog, no local NZB). Directories show as needing upload if any child does.
- A summary line: `N items · M unbacked · X GB to upload`.

### 40c-5 — Persist the queue; tell the truth about pause ✅ Done

> Implemented.
>
> **Queue persistence:** the queue is saved to
> `<config_dir>/upapasta-queue.json` on every mutation (queue/unqueue, remove,
> clear, reorder, and the post-upload prune) and restored at startup
> (`App::load_queue`). Paths that no longer exist on disk are dropped on load
> (with a status note) and the pruned list is re-saved, so a carefully built
> selection survives navigating away or restarting the app.
>
> **Pause removed (honestly):** `pesto`'s poster exposes only a cancel flag
> (`AtomicBool`), no suspend mechanism, so a real mid-file pause is impossible
> today. Rather than keep a button that lies, the `p` key, the `upload_paused`
> state, the `PauseUpload`/`ResumeUpload` events, and all "PAUSED" UI were
> removed. A true pause requires poster-level support and is tracked as a
> follow-up (see Phase 46).

### 40c-6 — Reconcile this roadmap with reality ✅ Done

> **Decision:** the queue gets its **own dedicated tab**. Selection still happens
> in the Browser (`Space`), but all queue *management* now lives in one place.
>
> **Real tab order (F1–F6):** `Dashboard · Queue · Browser · History ·
> NZB Vault · Config`. `Tab`/`Shift+Tab` cycle the same order.
>
> The new **Queue screen (F2)** is full-height and shows, per entry, the live
> status (`○ queued / ▶ uploading / ✓ done / ✗ failed`), the resulting NZB name,
> the bundled file count for folders, and the size — with a total in the title.
> Queue management keys (`u` upload, `d` remove, `c` clear, `J/K` reorder) moved
> off the Dashboard to the Queue screen, so the queue is no longer built in one
> place and managed in another. The Dashboard is now purely the live
> upload-progress + log view. Edits are blocked while an upload is running.

---

## Phase 40d — Upload-options workflow ✅ Done

A review of "how does a user change an option?" exposed three problems, all now
fixed in the upload-config panel (opened with `u` from Browser or Queue):

- **The arrow keys lied.** The panel hinted `←→ cycle` on Obfuscate/Verify, but
  only PAR2 responded; you had to press `Enter`. Now `←→` (and `Space`) advance
  every cycle/number/toggle field, matching the hint. The whole field list is
  driven by one `ConfirmField` enum, so the render and the key handlers can no
  longer disagree on order or behaviour.
- **The panel was too thin.** It only exposed 5 settings. It now covers
  Compress (format + password), From, Category and Article size as well — all of
  which `effective_config_with_overrides` already applied — plus a one-line
  legend explaining what the current obfuscation mode actually hides. The new
  persistent settings are restored across sessions like the others.
- **Folder/season mode was impossible.** `pesto`'s `--season` (per-episode NZBs
  **plus** a combined season NZB) had no equivalent in the TUI. A **Folder mode**
  field now appears whenever a directory is queued, with three options:
  - `single NZB` — the whole folder as one release (default, unchanged);
  - `per-file` — one NZB per file inside;
  - `season` — per-file NZBs **and** a combined season NZB, built in upapasta via
    `pesto::nzb::generate` over the segments each file posted.
  Each produced NZB is recorded in the catalog honestly (real size + path) via a
  per-NZB `CatalogRecord`, so a folder in per-file/season mode yields several
  truthful catalog rows instead of one. Folder mode is intentionally **per-batch**
  (resets to `single` each session) so an old `season` choice can never silently
  change how a folder uploads later.

> Known follow-up: the combined season NZB is recorded and saved locally but not
> yet pushed to the indexer (the per-episode NZBs are). Tracked for later.

---

## Phase 41 — TUI Layout Redesign (Current Priority)

### 41a — Three-pane Browser with NZB Status Column

Replace the current two-tab (Browser + History) model with a unified
**three-pane layout** that keeps file system and NZB status always visible.

```
┌─────────────────────────────────────────────────────────────────────┐
│  upapasta v2  | [F1 Dash] [F2 Queue] [F3 Browser] [F4 Hist] [F5 …] │
├──────────────────────┬────────────────┬────────────────────────────┤
│  FILESYSTEM          │  NZB STATUS    │  DETAIL / LOG              │
│  ~/Videos/           │                │                            │
│  ▶ 📁 Series/        │  ✓ uploaded    │  (shows selected file's    │
│    📄 Movie.mkv      │  ✓ 2024-03-01  │   NZB info or upload log)  │
│  ▶ 📄 Doc.mp4      ◀ │  ○ pending     │                            │
│    📄 Music.flac     │  — no nzb      │                            │
│    📄 Archive.zip    │  ✓ has local   │                            │
│                      │                │                            │
├──────────────────────┴────────────────┴────────────────────────────┤
│  STATUS BAR — speed | eta | phase | keybindings hint               │
└─────────────────────────────────────────────────────────────────────┘
```

**NZB Status indicators (pane 2):**
- `✓ uploaded` — in catalog, local NZB file present
- `✓ catalog`  — in catalog, NZB path missing from disk (warn on hover)
- `○ queued`   — in upload queue
- `▶ uploading` — actively being uploaded (animated)
- `— no nzb`  — not in catalog, no known NZB
- `? prowlarr` — found via Prowlarr search, not yet verified

**NZB badge in pane 2 shows upload type:**
- `[pub]`  — public subject, no obfuscation
- `[obf]`  — obfuscated subject
- `[full]` — full obfuscation (subject + poster + filename)
- `[🔒]`   — password protected
- `[🔒obf]` — obfuscated + password

### 41b — Upload Configuration Panel (pre-upload modal redesign)

Current modal is functional but cramped. Replace with a dedicated
**Upload Config Panel** that slides in from the right when `u` is pressed.

Fields the user controls per-batch:
- Obfuscation: None / Subject / Full
- Password: optional (shown obfuscated, toggleable reveal with Tab)
- PAR2 redundancy: 0–50% (slider or typed)
- Usenet groups: editable list (add/remove)
- Post-from address
- Compress: yes/no + password
- Article size: advanced toggle
- Verify after upload: yes/no

The panel must be keyboard-navigable (j/k, Enter, Esc) and show a live
summary of what will happen before the user confirms.

### 41c — Upload Progress Screen (redesign)

When an upload is in progress, replace the dashboard partial overlay with a
**full-height progress screen** (reachable via F1 or auto-switched on start).

```
┌──────────────────────────────────────────────────────────────────────┐
│  UPLOADING  Movie.mkv  [Ctrl+X cancel]  [p pause]                   │
├──────────────────────────────────────────────────────────────────────┤
│  COMPRESSION          ████████████████░░░░░░░  78%  1.2 GB / 1.6 GB │
│  PAR2 GENERATION      ████████░░░░░░░░░░░░░░░  34%  10% redundancy  │
│  UPLOAD               ████████████████████░░░  91%  45 MB/s  ETA 2m │
├──────────────────────────────────────────────────────────────────────┤
│  UPLOAD SPEED ▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆▇ (sparkline last 60s)           │
├──────────────────────────────────────────────────────────────────────┤
│  PER-FILE PROGRESS                                                   │
│  Movie.mkv.001   ▶  ████████████░░░░  58%                           │
│  Movie.mkv.002      ░░░░░░░░░░░░░░░░   0%  (queued)                 │
│  Movie.nzb.par2     ░░░░░░░░░░░░░░░░   —  (after upload)            │
├──────────────────────────────────────────────────────────────────────┤
│  LOG  [/ search]  [a auto-scroll]                                    │
│  14:23:01  Posted article 1842/2048  group alt.binaries.example      │
│  14:23:02  Article 1843 accepted                                     │
└──────────────────────────────────────────────────────────────────────┘
```

Rules:
- The three bars (Compress, PAR2, Upload) are always visible, greyed out
  when their phase has not started yet.
- Compress and PAR2 bars fill in advance (they pipeline).
- Upload is the **primary** bar — largest, most prominent.
- Sparkline shows last 60 seconds of upload speed.
- Per-file list shows filename, current status, individual gauge.
- Log panel below is scrollable but not the focus during active upload.

---

## Phase 42 — NZB Vault (Local NZB Folder Browser)

A dedicated screen listing all `.nzb` files in the folder configured in
`pesto.toml` (field `nzb_dir` or similar).

```
[F5 NZB Vault]
```

Features:
- Lists all `.nzb` files in the configured NZB directory.
- Parses each NZB on demand (lazy) to show: name, segment count, total size,
  group list, meta-password, obfuscation hint (if any).
- Cross-references catalog: shows whether this NZB corresponds to a known upload.
- Actions: `v` view NZB contents, `o` open in pager, `d` delete, `Enter` show detail.
- Sort by: date modified, size, name (toggleable).

**What this is NOT:** a download client. The vault is read-only browsing +
deletion. Users manage the NZB directory themselves (or via hooks).

---

## Phase 43 — Prowlarr / Indexer Integration

Passive integration only: search, verify, archive.

### 43a — Configuration

Add to `pesto.toml` or `upapasta.toml`:
```toml
[prowlarr]
url = "http://localhost:9696"
api_key = "abc123"
```

Config screen gains a Prowlarr section showing connection status.

### 43b — Search by Filename

From the Browser or NZB Vault, the user can press `P` (search Prowlarr)
on a selected file. UpaPasta:
1. Queries Prowlarr with the filename (and optionally hash/size).
2. Shows results in an overlay panel: indexer name, NZB title, age, size.
3. User selects a result and presses `d` to download the `.nzb` file to
   the configured NZB directory.
4. The NZB file itself is saved. No content download is initiated.

### 43c — Article Availability Check

For any NZB (local or from Prowlarr), the user can trigger an
**availability check** (`c` key). UpaPasta:
1. Parses the NZB and extracts a sample of Message-IDs.
2. Checks each sampled article via NNTP STAT command (no body download).
3. Reports: `N/M articles present (X%)` with a pass/fail indicator.
4. Result is saved to catalog alongside the NZB record.

This is implemented via `pesto`'s NNTP connection pool (read path only).

### 43d — Automated NZB Backup Workflow

Optional: when a local file has no NZB in catalog and no NZB on disk,
upapasta can search Prowlarr automatically and mark candidates.

The user must confirm before any NZB is downloaded. No auto-downloads.

---

## Phase 44 — Catalog Enhancements

- **Tags**: user-defined tags on upload records (e.g. `tv`, `4k`, `archived`).
- **Search improvements**: filter by date range, category, tag, group.
- **Bulk actions**: delete catalog records, reassign NZB paths.
- **Export**: export catalog to CSV or JSON for external tools.
- **Duplicate detection**: warn when uploading a file already in catalog.
- **Orphan detection**: flag catalog records whose NZB file no longer exists.

---

## Phase 45 — Metadata Enrichment (Optional / Later)

- TMDb lookup by filename: suggest movie/show metadata for the catalog record.
- Show poster art via Sixel/Kitty protocol if terminal supports it.
- NFO file generation per upload (for completeness with NZB).

This phase is lower priority. Do not block earlier phases on it.

---

## Phase 46 — Multi-Server, Retry & Real Pause

- Support posting to multiple servers simultaneously (primary + fill servers).
- Retry failed articles automatically with exponential backoff.
- Per-server connection health indicators in the Config screen.
- Alert when a server connection is degraded during upload.
- **Real pause/resume:** requires a suspend mechanism in `pesto`'s poster
  (hold the connection pool without tearing it down). Removed from the TUI in
  40c-5 until the engine supports it — do not re-add a UI-only pause.

---

## Decisions & Non-Goals

### What upapasta MUST NOT do
- Download Usenet article bodies (media files). That is a downloader's job.
- Manage `.nzb` imports into external downloader clients (SABnzbd, NZBGet).
  If a user wants that, they wire it via a post-upload hook.
- Index or transcode media. TMDb lookups are metadata-only.
- Replace a full Usenet client.

### What upapasta SHOULD do
- Be the best tool for uploading files to Usenet with PAR2.
- Maintain a trustworthy local record of everything ever uploaded.
- Verify that uploads are still retrievable years later.
- Keep the NZB archive clean, annotated, and cross-referenced.

### Design constraints
- Every feature must work without Prowlarr configured.
- The TUI must remain keyboard-only operable — no mouse requirement.
- All blocking I/O (NNTP, disk, Prowlarr HTTP) must run on tokio tasks,
  never on the render loop.
- Terminal compatibility: xterm-256color minimum. Sixel/Kitty optional.

---

## Keybinding Map (Target State)

| Key         | Context          | Action                              |
|-------------|------------------|-------------------------------------|
| F1–F6       | Global           | Jump to tab (Dash/Queue/Browser/Hist/Vault/Config) |
| Tab / S-Tab | Global           | Cycle tabs                          |
| j / k       | Lists            | Move cursor                         |
| Enter       | File tree        | Enter directory / open detail       |
| Space       | File tree        | Queue/unqueue item (file or folder) |
| b / Bksp    | File tree        | Go to parent directory              |
| h           | File tree        | Toggle hidden files                 |
| n           | Browser          | Toggle unbacked-only filter         |
| u           | Browser, Queue   | Open upload config panel for queue  |
| d / Del     | Queue            | Remove item from queue              |
| c           | Queue            | Clear queue                         |
| Shift+J/K   | Queue            | Reorder items                       |
| x           | Queue, Dashboard | Cancel running upload               |
| /           | Log, History     | Start search                        |
| Esc         | Any              | Close modal / cancel search         |
| P           | Browser, Vault   | Search Prowlarr for selected file   |
| v           | NZB Vault        | View NZB contents                   |
| s           | History          | Toggle stats panel                  |
| ?           | Global           | Show keybinding help overlay        |

---

## Milestones

| Milestone | Description                                      | Status    |
|-----------|--------------------------------------------------|-----------|
| 40b       | Core upload flow, progress bars, catalog, history| ✅ Done    |
| 40c-1     | One coherent selection model (queue = single source of truth) | ✅ Done |
| 40c-2     | Folder = one release = one NZB (with per-entry preview)        | ✅ Done |
| 40c-3     | Honest sequential queue + honest catalog (per-item record, real size, retry) | ✅ Done |
| 40c-4     | "What should I upload?" filter (unbacked items)              | ✅ Done |
| 40c-5     | Persist the queue; remove the fake pause                     | ✅ Done |
| 40c-6     | Dedicated Queue tab; layout reconciled with reality          | ✅ Done |
| 40d       | Upload-options workflow: ←→ fix, richer panel, folder/season mode | ✅ Done |
| 41a       | Three-pane browser with NZB status column        | ✅ Done    |
| 41b       | Upload config panel redesign                     | ✅ Done    |
| 41c       | Full-height upload progress screen               | ✅ Done    |
| 42        | NZB Vault screen                                 | ✅ Done    |
| 43a       | Prowlarr config + connection check               | ✅ Done    |
| 43b       | Prowlarr NZB search                              | ✅ Done    |
| 43c       | Article availability check (NNTP STAT)           | 🔲 Planned |
| 43d       | Automated Prowlarr backup workflow               | 🔲 Planned |
| 44        | Catalog enhancements (tags, export, dedup)       | 🔲 Planned |
| 45        | TMDb metadata enrichment                         | 🔲 Later   |
| 46        | Multi-server posting + retry + real pause        | 🔲 Later   |
