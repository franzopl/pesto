# Password archive fixtures

These small archives contain synthetic bytes, not real media or user data.
The password is `fixture`, except `rar4-long-password.rar`, whose password is
`0123456789abcdef0123456789abcdef`.

The stored entry is `movie.mkv`: `TEST`, followed by `bytes(range(256)) * 64`,
followed by `END` (16,391 bytes). Tests use a mocked MediaInfo probe and mocked
article source. No archive tools, external services, or hooks run during tests.
The fixtures independently check key derivation, header password checks,
unaligned ranges, trailing padding, and CBC continuation between volumes.

Generated manually with official RAR 6.24 (RAR4), RAR 7.00 (RAR5), and
7-Zip 23.01, using:

```bash
# RAR4: substitute the RAR 6.24 executable for rar.
rar a -ep -ma4 -m0 -pfixture rar4-data.rar movie.mkv
rar a -ep -ma4 -m0 -hpfixture rar4-headers.rar movie.mkv
rar a -ep -ma4 -m0 -hp0123456789abcdef0123456789abcdef rar4-long-password.rar movie.mkv

rar a -ep -m0 -pfixture rar5-data.rar movie.mkv
rar a -ep -m0 -hpfixture rar5-headers.rar movie.mkv
rar a -ep -m0 -hpfixture -v4096b rar5-split.rar movie.mkv

7z a -m0=Copy -mf=off -pfixture -mhe=off 7z-data.7z movie.mkv
7z a -m0=Copy -mf=off -pfixture -mhe=on 7z-headers.7z movie.mkv
7z a -m0=LZMA2 -pfixture -mhe=on 7z-compressed.7z movie.mkv
```

Timestamps, salts, and initialization vectors can vary when regenerating.
