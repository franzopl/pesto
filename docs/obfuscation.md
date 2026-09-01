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
| `header-fragmented` | A header-only observer cannot associate segments, while a body-aware observer can group one physical file by its opaque yEnc name. | Extra header fragmentation without breaking conventional client assembly and PAR2 cleanup. |

`article` remains hidden and experimental. Its legacy alias `paranoid` keeps
the old strict behavior: Subject, From and yEnc name all change per article.
It is deliberately not a SABnzbd/NZBGet-compatible mode for multipart PAR2
repair and cleanup. Existing configurations therefore retain their privacy
semantics instead of being silently weakened.

## Exact artifacts

The quoted name below means the value inside Pesto's conventional
`"name" yEnc (part/total)` Subject. NZB 1.1 has no `file@name` attribute;
Pesto does not emit one.

| Artifact | `none` | `light` | `full-shared` | `full` | `header-fragmented` |
|---|---|---|---|---|---|
| NNTP Subject quoted name | canonical client path | release prefix/file suffix | release prefix/file suffix | random per file | random per article |
| yEnc `name=` | canonical client path | identical to Subject name | same release prefix plus independent suffix | independent random per file | independent random per physical file |
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
components, absolute paths, backslashes and non-ASCII components, and
serializes `/` separators. The [PAR2
specification](https://parchive.sourceforge.net/docs/specifications/parity-volume-spec/article-spec.html)
defines the core File Description name as ASCII and reserves the optional
`UniFileN` packet for a Unicode override. That is not a portable encoding
contract: NZBGet 26.1's bundled `par2cmdline-turbo` v1.4.0
[interprets the core bytes as Latin-1 and converts them to
UTF-8](https://github.com/nzbgetcom/par2cmdline-turbo/blob/333913c529dbaae07c88dcdd690564cb680a59ae/src/descriptionpacket.cpp#L87-L113),
and the tagged source tree has no `UniFileN` reader. Pesto therefore rejects
non-ASCII raw paths rather than silently restoring a corrupt name. Use
`--compress=7z` for Unicode source names: Pesto posts an ASCII external archive
name and preserves the Unicode tree inside the archive. Windows-native MultiPar
and QuickPar remain explicit external gates rather than assumed compatibility.

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
Subject names, content hashes, and 7z extraction. A follow-up public fixture
also removed article 4 from an eight-article, 64 KiB raw file, leaving its first
24 KiB intact: NZBGet downloaded recovery volumes, reported `SUCCESS/PAR`, and
restored the exact SHA-256 hash. This is the representative partial-file repair
gate; the earlier failure of a 4.5 KiB file with its first article missing was
the expected consequence of losing the complete 16 KiB par-rename fingerprint.

The same follow-up exposed three interoperability defects. The PAR2 cleanup
defect is addressed by the supported `header-fragmented` contract. Raw Unicode
paths now fail early with an actionable error instead of silently restoring
under a corrupt name, and compressed posts use an ASCII external archive name.
The archive root fix is verified for 7z/ZIP and RAR. The RAR regression test
uses the proprietary CLI when it is installed and is skipped in environments
where that external binary is unavailable:

- After the successful repair, six downloaded recovery volumes remained in the
  completed directory under opaque per-article yEnc names. NZBGet could use
  their packets but could not recognize every assembled file as PAR2 for final
  cleanup. `header-fragmented` uses one stable opaque yEnc name per physical
  file, including each PAR2 volume, while keeping Subject and From fragmented
  per article. This is intentionally not the legacy strict `article` mode.
- A raw FileDesc path `Árvore/legenda-ação.txt` retained the correct contents
  but became the mojibake path `Ãrvore/legenda-aÃ§Ã£o.txt`. Inspection of
  NZBGet's exact bundled `par2cmdline-turbo` tag confirmed an unconditional
  Latin-1-to-UTF-8 conversion and no `UniFileN` reader, so Pesto rejects it.
- A newly posted archive passed `SUCCESS/UNPACK`, canonical rename to
  `archive-source.7z`, extraction, and hash verification. Pesto passes
  absolute compressor inputs to prevent its private staging prefix from being
  stored as an archive root; 7z/ZIP and RAR tests assert that the input
  basename, rather than the staging prefix, is the archive root.

A controlled NNTP end-to-end run on 2026-09-01 posted a five-part
`header-fragmented` fixture, forced two `STAT 430` replies, then accepted the
two reposts. The seven captured articles retained one yEnc name for the
physical file while each repost received a fresh Subject and From. The run
completed and wrote its NZB, covering the check/repost identity contract over
the actual client/server protocol.

MultiPar and QuickPar are useful Windows compatibility gates for PAR2 path
handling, but they do not define the NNTP/NZB posting contract.

## Resume and correlation behavior

Resume state and version-2 spool entries persist the exact Subject, yEnc name,
From and Date used for each article. Check/repost reuses that identity in every
mode except `header-fragmented` and legacy `article`: both receive a new
per-article Subject and From after a confirmed miss; `header-fragmented` keeps
the file's yEnc name while legacy `article` rotates it too. Legacy obfuscated
resume state lacks enough information
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
ASCII canonical relative path in PAR2. Compressed posts use an ASCII archive
part name; the original directory tree (including Unicode names) lives inside
7z/RAR/ZIP and is restored by extraction, not by PAR2 renaming the archive
back into source files. Header encryption is archive-format dependent.

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
