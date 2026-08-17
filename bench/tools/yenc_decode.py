#!/usr/bin/env python3
"""Independent yEnc decoder for the wire round-trip check.

Reads the raw article bodies the mock NNTP server captured with --save-dir,
decodes each one, verifies its ``=yend pcrc32=`` trailer, and reassembles the
original file from the ``=ypart begin=/end=`` offsets.

The point is that this is *not* pesto's decoder. A round-trip through
``pesto::yenc::decode_part`` would pass even if both halves shared the same
misunderstanding of the spec; this is written from the yEnc draft
(<http://www.yenc.org/yenc-draft.1.3.txt>) with nothing but the standard
library, so agreement means the bytes on the wire are genuinely yEnc.

Usage: yenc_decode.py <articles_dir> <output_file>
Exit status is non-zero on any decode, CRC or coverage failure.
"""

import binascii
import os
import re
import sys

YBEGIN = re.compile(rb"^=ybegin\s+(.*)$")
YPART = re.compile(rb"^=ypart\s+(.*)$")
YEND = re.compile(rb"^=yend\s+(.*)$")


def parse_kv(line):
    """Parse a yEnc keyword line.

    ``name=`` is special: it is always last and its value may contain spaces,
    so it cannot be split on whitespace like the numeric keywords.
    """
    out = {}
    if b"name=" in line:
        head, name = line.split(b"name=", 1)
        out["name"] = name.decode("latin1").strip()
    else:
        head = line
    for token in head.split():
        if b"=" in token:
            k, v = token.split(b"=", 1)
            out[k.decode("latin1")] = v.decode("latin1")
    return out


def decode_body(data):
    """Undo yEnc escaping. Returns the raw bytes of one part."""
    out = bytearray()
    escaped = False
    for byte in data:
        if escaped:
            out.append((byte - 106) & 0xFF)
            escaped = False
        elif byte == 0x3D:  # '='
            escaped = True
        else:
            out.append((byte - 42) & 0xFF)
    return bytes(out)


def decode_article(raw):
    """Decode one article body into (begin_offset, bytes, file_size, name)."""
    lines = raw.split(b"\r\n")
    header = None
    part = None
    payload = []
    trailer = None

    for line in lines:
        if header is None:
            m = YBEGIN.match(line)
            if m:
                header = parse_kv(m.group(1))
            continue
        if part is None and line.startswith(b"=ypart"):
            part = parse_kv(YPART.match(line).group(1))
            continue
        if line.startswith(b"=yend"):
            trailer = parse_kv(YEND.match(line).group(1))
            break
        payload.append(line)

    if header is None or trailer is None:
        raise ValueError("article missing =ybegin or =yend")

    decoded = decode_body(b"".join(payload))

    expected_len = int(trailer.get("size", len(decoded)))
    if len(decoded) != expected_len:
        raise ValueError(f"part length {len(decoded)} != =yend size {expected_len}")

    # pcrc32 on a multi-part article, crc32 on a single-part one.
    crc_hex = trailer.get("pcrc32") or trailer.get("crc32")
    if crc_hex:
        actual = binascii.crc32(decoded) & 0xFFFFFFFF
        if actual != int(crc_hex, 16):
            raise ValueError(f"CRC mismatch: {actual:08x} != {crc_hex}")

    # =ypart begin= is 1-based (spec §2.2); file offsets are 0-based.
    begin = int(part["begin"]) - 1 if part else 0
    file_size = int(header["size"])
    return begin, decoded, file_size, header.get("name", "unknown")


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[-2], file=sys.stderr)
        return 2

    src_dir, out_path = sys.argv[1], sys.argv[2]
    articles = sorted(
        os.path.join(src_dir, f)
        for f in os.listdir(src_dir)
        if os.path.isfile(os.path.join(src_dir, f))
    )
    if not articles:
        print(f"no articles found in {src_dir}", file=sys.stderr)
        return 1

    file_size = None
    covered = []
    with open(out_path, "wb") as out:
        for path in articles:
            with open(path, "rb") as fh:
                raw = fh.read()
            try:
                begin, data, size, _name = decode_article(raw)
            except ValueError as exc:
                print(f"{os.path.basename(path)}: {exc}", file=sys.stderr)
                return 1
            if file_size is None:
                file_size = size
                out.truncate(size)
            out.seek(begin)
            out.write(data)
            covered.append((begin, begin + len(data)))

    # Every byte of the file must be covered exactly once: a decoder that
    # silently dropped an article would otherwise produce a plausible file
    # with a hole of zeroes in it.
    covered.sort()
    position = 0
    for start, end in covered:
        if start != position:
            print(f"coverage gap or overlap at offset {position} (next part starts at {start})",
                  file=sys.stderr)
            return 1
        position = end
    if position != file_size:
        print(f"decoded {position} bytes, =ybegin declared {file_size}", file=sys.stderr)
        return 1

    print(f"decoded {len(articles)} articles, {position} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
