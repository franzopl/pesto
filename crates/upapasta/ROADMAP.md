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

## Phase 41 — TUI Layout Redesign (Current Priority)

### 41a — Three-pane Browser with NZB Status Column

Replace the current two-tab (Browser + History) model with a unified
**three-pane layout** that keeps file system and NZB status always visible.

```
┌─────────────────────────────────────────────────────────────────────┐
│  upapasta  v2.x  |  [F1 Browser]  [F2 Queue]  [F3 History]  [F4 …] │
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

## Phase 46 — Multi-Server & Retry

- Support posting to multiple servers simultaneously (primary + fill servers).
- Retry failed articles automatically with exponential backoff.
- Per-server connection health indicators in the Config screen.
- Alert when a server connection is degraded during upload.

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
| F1–F5       | Global           | Switch tabs                         |
| j / k       | Lists            | Move cursor                         |
| Enter       | File tree        | Enter directory / open detail       |
| Space       | File tree        | Mark/unmark for queue               |
| b / Bksp    | File tree        | Go to parent directory              |
| h           | File tree        | Toggle hidden files                 |
| u           | Browser          | Open upload config panel for marked |
| U           | Browser          | Upload single file under cursor     |
| d / Del     | Queue            | Remove item from queue              |
| c           | Queue            | Clear queue                         |
| Shift+J/K   | Queue            | Reorder items                       |
| p           | Upload progress  | Pause / Resume                      |
| Ctrl+X      | Upload progress  | Cancel upload                       |
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
| 41a       | Three-pane browser with NZB status column        | ✅ Done    |
| 41b       | Upload config panel redesign                     | ✅ Done    |
| 41c       | Full-height upload progress screen               | ✅ Done    |
| 42        | NZB Vault screen                                 | ✅ Done    |
| 43a       | Prowlarr config + connection check               | ✅ Done    |
| 43b       | Prowlarr NZB search                              | 🔲 Planned |
| 43c       | Article availability check (NNTP STAT)           | 🔲 Planned |
| 43d       | Automated Prowlarr backup workflow               | 🔲 Planned |
| 44        | Catalog enhancements (tags, export, dedup)       | 🔲 Planned |
| 45        | TMDb metadata enrichment                         | 🔲 Later   |
| 46        | Multi-server posting + retry                     | 🔲 Later   |
