# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] — 2026-08-18

### Added

#### Phase 47 — Season Mode: Global PAR2

- **`--season` mode now generates a global PAR2 recovery set** that covers the entire season uniformly instead of separate rsids per episode
  - Eliminates confusion from multiple PAR2 recovery set IDs in consolidated season NZBs
  - Single coherent rsid ensures compatibility with all Usenet downloaders (SABnzbd, NZBGet, etc.)
  
- **New public API functions** in `pesto::poster`:
  - `pub async fn generate_season_par2()` — generates recovery slices covering all episodes in one pass (single file re-read)
  - `pub async fn generate_and_write_season_par2()` — generates and writes PAR2 volumes to disk
  
- **Automatic integration into `--season` consolidation**:
  - After all episodes post individually with their own PAR2 sets, pesto generates a global PAR2 covering the entire season
  - Posts PAR2 volumes as NNTP articles
  - Includes global PAR2 in consolidated season NZB (filters out per-episode PAR2 sets)
  - Result: single season NZB with unified PAR2 recovery
  
- **Behavior**:
  - Individual episode NZBs remain unchanged (data + local PAR2 per episode)
  - Season NZB contains: all episode data + global PAR2 volumes only (single rsid)
  - Non-fatal: if global PAR2 generation fails, falls back to per-episode PAR2 sets
  
- **Documentation**: Updated `--obfuscate=full-shared` mode in README with indexer compatibility notes

### Fixed

- Season NZB consolidation now correctly filters out per-episode PAR2 sets to avoid multiple rsids
- PAR2 volume posting no longer generates recursive PAR2 for the volume files themselves
- The internal PAR2-only NZB used to post the season's global PAR2 volumes no longer leaks onto disk as an orphaned temp file — it used to be written to a path distinct from the one actually cleaned up on drop, so it could persist and get submitted to an indexer ahead of the real season NZB
- That same internal PAR2-only upload no longer runs the user's configured `post_hooks` — `no_hooks` only ever suppressed the hooks-directory scan, so a configured indexer-submission hook still fired against it
- The season's global PAR2 now covers the files actually posted (the compressed archive, under `--compress`/`--password`) instead of the original, never-posted episode files, whose temp archive used to be deleted before the season PAR2 step could read it
- The season's own PAR2 volumes no longer get wrapped in a password-protected archive themselves when `--compress`/`--password` is active — they're posted as raw, readable `.par2` files like any other PAR2 set

### Tested

- Validated with small season (3 × 5 MB episodes): ✓ single global rsid
- Validated with large season (10 × 10 MB episodes): ✓ 157 segments with single coherent rsid
- Validated with 26-episode season (6.2 GB): ✓ single global rsid with 99 recovery blocks
- Individual episodes still independently recoverable via their own NZBs
- Season-wide recovery via global PAR2 set
- SABnzbd verification: ✓ all files correct, repair not required

### Notes

- **PAR2 File Cleanup**: SABnzbd's automatic PAR2 cleanup (`enable_par_cleanup`) only works for simple downloads (1 file : 1 PAR2). Season mode creates a complex download (multiple files : 1 PAR2 set), so SABnzbd does not delete PAR2 files automatically. Workaround: keep Radarr/Sonarr active — it automatically deletes PAR2 when moving files to the library.
- **Backwards Compatible**: NZBs created before Phase 47 still work correctly with their individual per-episode PAR2 sets.

---

## Historical Versions

Refer to git tags for previous releases: `v0.5.10` and earlier.
