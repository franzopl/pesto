# Obfuscation contracts

Obfuscation changes discoverability and correlation metadata. It is not
encryption, anonymity, or a retention control. A party that has the NZB can
fetch the articles, and a party that fetches a recovery volume can read its
PAR2 File Description packets.

The governing question for every mode is: **who can assemble the release
without its NZB?**

| Mode | One-sentence contract | Reason to exist |
|---|---|---|
| `none` | A header indexer can discover and assemble files by their real paths. | Standards-friendly public posting. |
| `light` | A header/body indexer can group the release through a shared prefix and the deliberate Subject = yEnc-name signal. | Compatibility with indexers that require the exact-match fingerprint. |
| `full-shared` | A header/body indexer can group the release through a shared prefix without Subject equalling yEnc `name=`. | Discoverability without that exact-match fingerprint. |
| `full` | The NZB is required to connect files, while all parts of one file retain one wire identity. | Private default with conventional per-file yEnc assembly. |
| `article` *(experimental, hidden)* | The NZB is required even to associate the segments of one file. | Maximum metadata fragmentation, pending client interoperability gates. |

`paranoid` remains an accepted configuration and CLI alias for `article` so
existing configurations do not break. New configuration and hook output use
`article` because it describes the mechanism without overstating its privacy.

## Exact artifacts

The quoted name below means the value inside Pesto's conventional
`"name" yEnc (part/total)` Subject. NZB 1.1 has no `file@name` attribute;
Pesto does not emit one.

| Artifact | `none` | `light` | `full-shared` | `full` | `article` |
|---|---|---|---|---|---|
| NNTP Subject quoted name | canonical client path | release prefix/file suffix | release prefix/file suffix | random per file | random per article |
| yEnc `name=` | canonical client path | identical to Subject name | same release prefix plus independent suffix | independent random per file | independent random per article |
| From | configured value | random per release | random per release | random per file | random per article |
| Date | omitted unless requested | same | same | same | same |
| Message-ID | 128 random bits as 32 lowercase hex digits | same | same | same | same |
| Message-ID domain | random per article unless configured | same | same | same | same |
| Standalone PAR2 index posted | yes | yes | yes | no | no |
| Recovery volumes posted | yes | yes | yes | yes | yes |
| PAR2 FileDesc name | canonical client path using `/` | same | same | same | same |
| NZB Subject quoted name | canonical client path | same | same | same | same |

The generated NZB's quoted Subject name is intentionally the client path in
every mode. SABnzbd initially names the download from that value. NZBGet can
also use it when the yEnc name looks obfuscated. PAR2 FileDesc remains the
authoritative rename source for both clients and therefore carries the same
relative path.

Pesto strips one common outer input directory because the downloader creates
the job directory, preserves every directory below it, rejects `.`/`..`, empty
components, absolute paths and backslashes, and serializes `/` separators.
FileDesc names are UTF-8 bytes. This interoperates with the repository's
par2cmdline Unicode round-trip test, but Parmesan does not currently emit the
optional PAR2 Unicode filename (`UniFileN`) packet. Windows-native MultiPar,
QuickPar, SABnzbd and NZBGet Unicode/path behavior remains an explicit external
test gate rather than an assumed guarantee.

## PAR2 layout

Discovery modes post the conventional small index plus recovery volumes.
Private modes do not post the standalone index. Every recovery volume already
starts with Main, Creator, all FileDesc packets and all IFSC packets, followed
by its recovery slices. Consequently the first volume is independently usable
as PAR2 metadata and there is no special `vol0` format.

No padding is added. Padding a small PAR2 artifact only defeats a size filter;
it does not remove its FileDesc names. Omitting the standalone copy removes the
cheap one-part sidecar while retaining normal PAR2 semantics. This is not
cryptographic concealment: an indexer that downloads any recovery volume can
still read the real paths and 16 KiB hashes.

The automated compatibility gate proves `par2cmdline` can verify and repair
using a recovery volume after the index is deleted. A live SABnzbd 5.1.1 test
on 2026-09-01 also passed raw-file reassembly, 7z extraction, nested UTF-8
paths, repair of a deliberately missing data article using recovery volumes
only, and FileDesc rename from deliberately false NZB Subject names.

A live NZBGet 26.1 test on the same date, with `FileNaming=auto`,
`ParRename=yes`, `ParCheck=auto` and `Unpack=yes`, passed multi-segment
`article` reassembly, volume-only FileDesc rename from deliberately false NZB
Subject names, content hashes, and 7z extraction (including a UTF-8 path inside
the archive). It also found two remaining raw-file gates, so `article` remains
hidden:

- Without `UniFileN`, NZBGet double-transcoded a UTF-8 FileDesc path: the file
  contents were correct, but `Árvore/legenda-ação.txt` was written with the
  mojibake path `Ãrvore/legenda-aÃ§Ã£o.txt`.
- Removing the first article from a 4.5 KiB file prevented NZBGet's 16 KiB MD5
  par-rename match. It consequently treated the whole file as missing and the
  37 available recovery blocks could not supply the 71 blocks requested. A
  fixture larger than 16 KiB with damage after the matching prefix is still
  required to exercise ordinary partial-file repair.

Before `article` is unhidden, the remaining end-to-end matrix is therefore:

- emit and consume interoperable Unicode PAR2 path metadata on NZBGet (and
  retain the already-passing SABnzbd behavior);
- repair a larger raw file after an article beyond its first 16 KiB is missing,
  using recovery volumes only;
- download and extract a newly posted archive produced after the canonical
  archive-name fix, rather than relying on the passing pre-fix archive fixture;
- cover a missing article's repost/check identity in a live posting run.

MultiPar and QuickPar are useful Windows compatibility gates for PAR2 path
handling, but they do not define the NNTP/NZB posting contract.

## Resume and correlation behavior

Resume state and version-2 spool entries persist the exact Subject, yEnc name,
From and Date used for each article. Check/repost reuses that identity in every
mode except `article`, where a confirmed-missing copy receives a complete new
per-article identity. Legacy obfuscated resume state lacks enough information
to extend a file without violating its contract and is rejected with a
migration error. Legacy spool bytes without identity metadata are ignored and
re-encoded. Existing NZBs and old Message-IDs remain readable because download
and STAT paths continue treating Message-ID as opaque.

Private modes reject the release-wide file counter because it directly links
otherwise unrelated files. Discovery modes retain it. These modes do not try
to disguise Pesto's other defaults—768000-byte articles, 128-character yEnc
lines, `(part/total)` Subject grammar, timing and general From shape can all be
classifier inputs. Changing those defaults merely to chase an indexer would
create more knobs without an honest privacy boundary.

Compression changes what FileDesc describes. Raw-file posts store each input's
canonical relative path in PAR2. Compressed posts protect and restore the
archive part names; the original directory tree lives inside 7z/RAR/ZIP and is
restored by extraction, not by PAR2 renaming the archive back into source
files. Header encryption is archive-format dependent.

## Explicit non-goals

- Retention promises: `X-No-Archive` is only a request to avoid archival and
  no NNTP header can extend a provider's spool retention.
- Dual PAR2 sets or NZB-only PAR2 metadata.
- Inventing an NZB attribute outside the NZB 1.1 DTD.
- Obfuscating FileDesc names: doing so breaks SABnzbd/NZBGet PAR2 rename and
  nested-directory restoration.
- Padding recovery metadata to evade current indexer size heuristics.
- Claiming that random headers prevent traffic, timing, size or content-based
  correlation.
