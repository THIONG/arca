# Arca

Cross-platform archiver in Rust. **Phase F01 and part of F03**: core, ZIP and TAR,
Zstandard, multi-threaded compression and a command line.

The container parsers are written in safe Rust with `#![forbid(unsafe_code)]` at
crate level. A malformed archive produces an error, never memory corruption.

## Build

```sh
cargo build --release      # binary at target/release/arca
cargo test --workspace     # 45 tests
bash interop.sh            # the phase acceptance criterion
```

Two profiles, as section 03 of the design lays down:

```sh
cargo build --release                              # codecs-native: includes libzstd (C)
cargo build --release --no-default-features        # pure Rust: builds for any target
```

## Use

```sh
arca create copy.zip my-files/ -l normal        # store | fast | normal | best
arca create copy.zip my-files/ -c zstd          # auto | store | deflate | zstd
arca create copy.zip my-files/ -j 8             # threads; 0 = every core
arca create copy.tar.gz my-files/
arca create copy.zip my-files/ -p secret        # encrypts with AES-256
arca list copy.zip --time
arca extract copy.zip -o where/
arca extract copy.zip -o where/ -p secret       # encrypted archive
arca extract copy.zip -o where/ -j 8            # threads; 0 = every core
arca password copy.zip -p secret                # takes the password off
arca password copy.zip --new secret             # puts one on
arca password copy.zip -p old --new new         # changes it
arca test copy.zip                              # checks the CRC without writing to disk
arca bench copy.zip                             # measures requirements R1 and R2
```

Short aliases: `c`, `l`, `x`, `t`.

## Layout

| Crate | What it does | `unsafe` |
|---|---|---|
| `arca-core` | Errors, limits, bounded header reads, MS-DOS dates, Zip Slip defence | forbidden |
| `arca-zip` | ZIP with Zip64; store, deflate over zlib-rs, Zstandard, AES-256 | forbidden |
| `arca-tar` | ustar TAR with checksum verification | forbidden |
| `arca-cli` | The `arca` binary | allowed, unused |
| `arca-gui` | The `arca-gui` window | forbidden |
| `windows/arca-shell` | Explorer context menu. Outside the workspace so `cargo build` still works on Linux and macOS | required: COM |

## Measured

The first set is from a Xeon at 2.80 GHz, 2 cores, Linux.

| Requirement | Target | Measured | |
|---|---|---|---|
| R1 cold start | < 15 ms | **1.6 ms** | unzip 2.0 · 7z 3.7 |
| R2 list 6000 entries | < 200 ms | **4.5 ms** | unzip 17.2 · 7z 45.4 |
| R3 scaling to 2 threads | ≥ 1.6× | **1.89×** | 94 % efficiency |

Compressing 81 MB across 120 separate files, 2 threads:

| | Time | Size |
|---|---|---|
| **Arca, zstd** | **330 ms** | **26.16 MB** |
| Arca, deflate | 941 ms | 28.35 MB |
| `zip -6` | 3934 ms | 27.65 MB |
| 7-Zip zip, 2 threads | 5358 ms | 26.88 MB |

With Zstandard, Arca is 11.9× faster than `zip` and 16.2× faster than 7-Zip on
that machine, and produces the smallest archive of the four. With deflate it is
4.2× faster than `zip` at the cost of 2.5 % in size: the known zlib-rs trade.

Those numbers come from a different machine and should not be read as a general
advantage. Measured on Windows 11 with 16 threads, deflate against deflate, best
of 3:

| Corpus | Arca | 7-Zip `-mx5` | Arca size | 7-Zip size |
|---|---|---|---|---|
| 5358 source files, 54.8 MB | 0.710 s | **0.627 s** | 13,729,308 | 13,747,705 |
| 16 files, 287 MB | **0.402 s** | 2.285 s | 56,757,284 | 54,753,868 |

```
arca create c1.zip src
7z a -tzip -mx5 c2.zip src
```

So: with large files Arca compresses 5.7× faster at the cost of 3.7 % in size,
and with many small files 7-Zip is somewhat ahead at the same size. There is no
single winner; it depends on the corpus.

### Parallel extraction

A `.zip` is random access: the central directory says where every entry starts,
so one thread per core can each open the file and decompress a different entry.
A `.tar` is a single stream and there is nothing to split.

Measured on Windows 11, 16 threads, over 287 MB in 16 text files:

| | Time |
|---|---|
| **Arca, 16 threads** | **0.207 s** |
| Arca, `-j 1` | 0.686 s |
| 7-Zip | 1.020 s |

```
arca create big.zip big
arca extract big.zip -o out -j 1
arca extract big.zip -o out
7z x -o"out" big7z.zip
```
Best of 3, deleting `out` before each pass.

Over many small files the split changes nothing, and it is worth saying why:
extracting 5358 source files takes 4.5 s, but decompressing those same 55 MB
takes 0.128 s (`arca test bench.zip`). 97 % of the time goes into creating files
on NTFS, not into decompressing. 7-Zip takes the same 4.6 s because it hits the
same wall.

### Against NanaZip, on an archive big enough to hurt

The corpus above is small enough to sit in the operating system's write cache,
which flatters everybody. This one does not: a 3.13 GB `.zip` holding 1513
files that unpack to 6.28 GB, all deflate, with 13 entries over 100 MB and the
five largest adding up to 2.77 GB. Windows 11, 16 cores, NanaZip 7.0.1832, and
a Kingston A400 — a DRAM-less SATA SSD.

First, decompression on its own. Both tools read every entry, check every CRC
and write nothing, so the disk is out of the picture. Both run on one core:

| | Wall | CPU | Cores |
|---|---|---|---|
| **Arca** | **13.2 s** | 13.2 s | 1.0 |
| NanaZip | 25.9 s | 25.9 s | 1.0 |

```
arca test big.zip
NanaZipC t big.zip
```

Same work, same one core, no disk: **the deflate decoder is 2× faster**. Three
runs each landed within 0.3 s.

Then the whole job, writing all 6.28 GB out. Run A B B A so that whatever the
disk does over time cannot favour either side, with a plain 6 GB sequential
write before and after as a control:

| Order | | Time |
|---|---|---|
| 1st | **Arca** | **21.9 s** |
| 2nd | NanaZip | 49.0 s |
| 3rd | NanaZip | 68.0 s |
| 4th | **Arca** | **28.0 s** |

```
arca extract big.zip -o out
NanaZipC x -y -o"out" big.zip
```

The control says why the order matters: the disk sustained **327 MB/s before
the four runs and 90 MB/s after them**. Arca took the first slot and the last,
so it ran on both the freshest and the most worn disk; NanaZip got the two in
between. Arca's *worst* run still beats NanaZip's *best*.

### Why Arca wins here, and it is not the disk

Measuring processor time during a real extraction separates the two possible
explanations. Its own pair of runs, so the wall times need not match the table
above; what matters is the shape:

| | Wall | CPU | Cores |
|---|---|---|---|
| **Arca** | **19.4 s** | 22.0 s | 1.1 |
| NanaZip | 31.6 s | 30.1 s | 1.0 |

Arca is not winning by spreading the work wider: it uses 1.1 cores against 1.0.
What the table shows instead is that NanaZip's wall time and its processor time
are the same number, which is what a program looks like when its own CPU is the
limit — the disk never gets to be its problem. Arca burns more CPU than wall
clock, so decompression is happening while bytes are going out, and the wall
clock settles against the disk: 6.28 GB in 21.9 s is 294 MB/s, against a
measured ceiling of 327 MB/s.

So the faster decoder is the cause and saturating the disk is the consequence,
not the other way round. It also means this machine shows Arca at its worst:
those 16 threads are mostly parked waiting for writes, and on a drive that
could take the bytes faster the gap would open up, because Arca's limit would
move and NanaZip's would not.

**On measuring this at all:** an ordered sweep over thread counts on this drive
produced a clean, convincing and completely false result — 1 thread looking
twice as fast as 16 — because the drive degrades as a run goes on and "more
threads" was really "later in the run". Re-running alternating, 16 threads beat
1 thread in all three rounds. Any benchmark here that writes several GB has to
alternate the arms and check the disk before and after, or it measures the SSD's
mood.

**A note on Zstandard in ZIP:** it is method 93, registered in the specification
but not yet read by classic `unzip`. That is why `-c auto` uses deflate in a
`.zip`: a zip exists so that anything can open it. Zstandard is asked for by
hand, and will be the default once there is a native format.

## Encryption

AES-256 in `.zip`, using the WinZip AE-2 scheme: PBKDF2-HMAC-SHA1 over 1000
rounds to derive the key, AES-256 in CTR mode, and an HMAC-SHA1 authenticating
the ciphertext. It is the same thing 7-Zip, WinRAR and NanaZip write, and
`interop.sh` checks it in both directions against 7-Zip.

Every entry carries its own random 16-byte salt. Reusing a salt across entries
would reuse the keystream, and two identical files would look identical inside
the archive.

Encryption happens after compression, which is the order the specification calls
for: the other way round the compressor would find nothing to compress. The CRC
is stored as zero, which is what AE-2 says: it is a checksum of the plaintext and
has no business being there once the HMAC speaks for the data.

An altered byte does not come out as content, it fails the authentication code.
Decryption is streaming, so that verdict arrives once the bytes are already
written: the caller has to throw away what it wrote if extraction fails.

What it does **not** do: ZipCrypto, the old password scheme, which is broken and
is neither read nor written. File names are not encrypted, because the ZIP format
does not allow it: the listing is visible without the password.

```sh
arca create secret.zip folder/ -p "a password"
arca extract secret.zip -o where/ -p "a password"
7z t -p"a password" secret.zip        # 7-Zip reads it
```

The window has a password field when creating, and asks for the password before
extracting when the archive it opens is encrypted.

### Changing the password of an existing archive

```sh
arca password secret.zip -p "a password"                 # takes it off
arca password plain.zip --new "a password"               # puts one on
arca password secret.zip -p "old" --new "new"            # changes it
arca password secret.zip -p "a password" -o clean.zip    # leaving the original alone
```

Nothing is compressed again. AES encrypts the already compressed bytes, so
removing the encryption gives back exactly the deflate stream that was there: it
goes straight through and the compressed size does not change. What it does cost
is the CRC, because an AE-2 entry stores zero there and the entry has to be
decompressed once to work it out before it can be written unencrypted.

Replacing in place destroys the only copy of the data, so the new archive is
built alongside the old one, read back in full to check it is sound, and only
then moved over it. If anything fails the original is left as it was, and no
temporary file is left behind.

In the window it is a button on the toolbar: **Remove password** when the open
archive is encrypted, **Set password…** when it is not.

## The window

Double clicking a folder goes into it; double clicking a file pulls that one
entry out to a temporary folder and hands it to whatever the system opens it
with. The whole row answers, not just the name, and the cursor says so.

The mouse back and forward buttons move through where you have been, and so do
Alt+Left and Alt+Right. The three arrows on the toolbar do the same thing, plus
one level up.

The list can be driven without a mouse at all. The keys act on the highlighted
row, and the first arrow press puts that highlight on the first row rather than
jumping blindly:

| | |
|---|---|
| ↑ ↓ · PageUp PageDown · Home End | move through the list |
| Enter | go into the folder, or open the file |
| Backspace | up one level |
| Space | tick or untick the row |
| Alt+← Alt+→ | back and forward |

While the filter box or a dialog has the keyboard, none of these apply: the
typing belongs there. The window also declares itself through AccessKit, so
Narrator and NVDA can read it.

Right clicking an archive in the Explorer opens the window with a progress bar
rather than running the command line with no console: an extraction that fails,
or that finds a file already there, now says so instead of doing nothing.

## Interoperability

`interop.sh` checks 34 cases, verifying the SHA-256 of the contents:

- What Arca writes is read by `unzip`, `tar` and 7-Zip, at all four levels
- What `zip`, `tar` and 7-Zip write is read by Arca without losing a byte
- An archive Arca encrypted with AES-256 opens in 7-Zip, and the other way round
- Adding and removing the password of an existing archive, Arca's own or 7-Zip's
- An altered byte is caught by the CRC, or by the HMAC when encrypted
- An entry with `../../` is rejected instead of writing outside the destination

## Windows

`windows/` holds the Explorer context menu extension: the modern Windows 11 menu
(`IExplorerCommand` plus a sparse MSIX package) and the classic one
(`IContextMenu` plus registry keys). Built, installed and verified on Windows 11.
The installer is produced with Inno Setup from `windows/arca.iss`. See
`windows/README.md`.

## Not yet

7z format, xz/LZMA2, symbolic links, GNU tar long names, solid archives
(compressing every file as one stream, which is where the largest ratio gain
comes from) and desktop integration on Linux and macOS.
