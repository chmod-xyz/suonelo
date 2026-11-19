# Windows x64 FLAC resource check

Run from the repository root with Rust's Windows x64 MSVC target and Xiph FLAC on PATH. Xiph is used only by the harness.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/measure-flac.ps1 -ShortSeconds 3 -LongSeconds 30
```

The harness builds the optimized, locked app test executable, then generates unequal deterministic signed stereo PCM at 24 bits and 192 kHz. Xiph FLAC encodes that PCM and the harness zeros the STREAMINFO MD5. Each native job runs the application's repair and final verification against a separate private stage. The baseline uses Xiph `-8 --threads=1 --verify` to re-encode, followed by `-t` on its output. For each native output the harness also runs Xiph `-t` and checks that the whole-file SHA-256 matches the original FLAC. The Xiph re-encoded outputs must reproduce the original PCM MD5. These comparisons are outside the timed phases.

Working set is the largest live process-group total observed at roughly 10 ms polling intervals. It includes the Rust test host or the live Xiph processes and can miss short-lived peaks. Elapsed time is wall time for each repair-and-verify batch, including process startup. Xiph encoding and testing are timed separately and their times are added. The Xiph peak is the larger peak of the two phases. Three submitted native jobs share a two-worker limit. The results cover synthetic audio on one Windows development host.

## Final implementation, 19 November 2026

The Windows x64 release-mode test executable had verified SHA-256 `a017c969118064becb98425a6ac0f4b8325f58f7a7bbb777755f199fd4b99030`.

| Audio seconds per job | Jobs | Native seconds | Native peak MiB | Xiph seconds | Xiph peak MiB |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1 | 0.085 | 9.70 | 0.102 | 6.65 |
| 3 | 3 | 0.135 | 10.36 | 0.125 | 21.89 |
| 30 | 1 | 0.798 | 9.71 | 0.538 | 15.29 |
| 30 | 3 | 1.274 | 10.43 | 0.755 | 45.84 |

All native repairs passed Xiph verification and restored the original file SHA-256. All Xiph re-encodes retained the original PCM MD5. The isolated worker-bound and cancellation test passed. The Windows release-mode suite passed 132 tests and reported 12 ignored. The harness ran three of those ignored test entry points separately.

At 30 seconds per job, native repair and verification took 1.48× the Xiph baseline for one job and 1.69× for three jobs. It used 63.5% and 22.8% of Xiph's sampled peak working set, respectively. The one-job 3-second run used more memory than Xiph (9.70 MiB versus 6.65 MiB). Native elapsed time at 30 seconds dropped from 25.422 to 0.798 seconds for one job and from 59.107 to 1.274 seconds for three jobs compared with the preliminary implementation. The sampled native working set increased by 0.01 MiB for one job and 0.07 MiB for three jobs across the tenfold duration increase in the final run.

A separate stress check used 24-bit, 192 kHz, 8-channel FLAC with 65,535-sample blocks, forged minimum and maximum compressed-frame size hints of `0xffffff`, and a cleared STREAMINFO MD5. The 2-second (338-byte) and 20-second (2,564-byte) files reached sampled peaks of 19.60 and 19.62 MiB respectively. Both repaired outputs passed Xiph `-t` and matched the original PCM MD5.

## Preliminary run on 19 November 2026

These measurements describe the earlier implementation, which was replaced before release.

Windows build 26300 on AMD64 with 12 available logical processors, Rust 1.99.1 and Xiph FLAC 1.5.0. The working tree was based on `09989ba` and contained the native FLAC changes. The test executable SHA-256 was `d4c8195a245c222627334ae3f8831eda6b40315d1730c4dc1549393d80cf4685`.

The isolated cancellation test reached two simultaneous blocking workers and held the third job behind the production semaphore. The queued job returned cancellation before either worker was released. The active job retained its permit until reaching its checkpoint and exiting. Independent production verification succeeded while the second worker still held its permit. This exercises worker scheduling and checkpoint cancellation without depending on the timing of a particular decode block.

| Audio seconds per job | Jobs | Native seconds | Native peak MiB | Xiph seconds | Xiph peak MiB |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 1 | 2.748 | 9.32 | 0.093 | 6.81 |
| 3 | 3 | 4.408 | 25.00 | 0.120 | 21.22 |
| 30 | 1 | 25.422 | 9.32 | 0.496 | 15.27 |
| 30 | 3 | 59.107 | 9.69 | 0.524 | 45.81 |

All native repairs passed Xiph verification and restored the original file SHA-256. All Xiph re-encodes retained the original PCM MD5. The sampled native working set did not grow with a tenfold increase in duration at either job count. Three submitted jobs used the two-worker native limit. On this input at 30 seconds per job, native repair and verification took 51× the Xiph baseline for one job and 113× for three jobs.


## Full PCM encoding comparison, 19 November 2026

On Windows x64 build 26300 (AMD Ryzen 5 7600X, 12 logical processors), an optimized standalone Rust 1.99.1 harness encoded signed, little-endian 24-bit stereo PCM at 192 kHz with `decibri-decode` 0.1.5. It converted each signed sample to `f32` by dividing by 8,388,608, then called `FlacWriter::new(AudioSpec::new(192_000, 2), 24).with_level(8).to_bytes(&samples)`. Xiph FLAC 1.5.0 encoded the same raw files with these options:

```powershell
flac.exe -s -8 --threads=1 --no-padding --no-seektable --force-raw-format --endian=little --sign=signed --channels=2 --bps=24 --sample-rate=192000 -o output.flac input.raw
```

The input used the same deterministic periodic stereo generator as the repair harness. Each duration had one excluded warm-up per encoder followed by three timed pairs, with the launch order alternating. Wall time includes process startup, PCM reading, any input conversion, encoding, and writing the output file. Input generation, decoding, and hashing were outside the timed interval. The two level-8 settings use their respective encoder presets. File sizes include the metadata each encoder wrote.

| Audio seconds | decibri median seconds | Xiph median seconds | decibri FLAC bytes | Xiph FLAC bytes |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 0.419 | 0.048 | 2,302,777 | 2,304,431 |
| 30 | 3.678 | 0.324 | 23,042,432 | 23,044,232 |

Xiph decoded all 16 outputs, including warm-ups, back to raw PCM with SHA-256 matching the input for that duration. Each encoder reproduced identical FLAC bytes across its four runs at each duration.

| Audio seconds | Input and decoded PCM SHA-256 |
| ---: | :--- |
| 3 | `114a4c3260637af6044a5ea65729fa840bccd0e01358fc23e553c3afb9908078` |
| 30 | `3c538f208482256cb738bffee4e60cd59a72469f6ac81bb306ea35b788327668` |

On these fixtures, decibri took 8.75× and 11.36× as long as Xiph and wrote 1,654 and 1,800 fewer bytes, respectively. These figures describe full encoding from PCM on one synthetic waveform. The production checksum repair decodes existing frames to compute the checksum, updates STREAMINFO, and preserves the encoded frames.

The standalone harness and per-run measurements are local under `src-tauri/target/verification/t25-flac-encode/`. The following commands were run from the repository root after loading the Visual Studio x64 build environment:

```powershell
cargo build --offline --release --target x86_64-pc-windows-msvc --manifest-path src-tauri/target/verification/t25-flac-encode/Cargo.toml
Copy-Item src-tauri/target/verification/t25-flac-encode/target/x86_64-pc-windows-msvc/release/encode.exe src-tauri/target/verification/t25-flac-encode/encode.exe
python src-tauri/target/verification/t25-flac-encode/benchmark.py
```
