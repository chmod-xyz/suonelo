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
