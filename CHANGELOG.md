# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

### Tested

- Validated with small season (3 × 5 MB episodes): ✓ single global rsid
- Validated with large season (10 × 10 MB episodes): ✓ 157 segments with single coherent rsid
- Individual episodes still independently recoverable via their own NZBs
- Season-wide recovery via global PAR2 set

---

## Historical Versions

Refer to git tags for previous releases: `v0.5.10` and earlier.
