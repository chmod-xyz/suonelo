# Native codec FLAC fixtures

These fixtures use original synthetic PCM generated for this project and encoded with Xiph FLAC 1.5.0. The fixture name gives bit depth, sample rate in Hz, and channel count. Each stream has 217 sample frames, a 192-sample silent block, and a short final block with signed extrema and unequal stereo channels.

To generate the PCM, write signed little-endian interleaved values. For zero-based frame `i < 192`, write zero for every channel. Otherwise, for zero-based channel `c`, take index `(i + c) % 4` from `[-2^(bits-1), 2^(bits-1)-1, -11-c, 17+3*c]`. Encode with `flac -V --force-raw-format --endian=little --sign=signed --channels=<channels> --bps=<bits> --sample-rate=<rate> --blocksize=192 --no-padding --no-seektable`.

Python's hashlib calculated the following PCM MD5 values independently of the codec. Sample rate changes do not alter the sample bytes.

| Bit depth | Channels | PCM MD5 |
| --- | --- | --- |
| 16 | 1 | `1fd770320c52f215e3ffe2d0e319e16c` |
| 16 | 2 | `b88837a2bd25079d0f84f1ccbedcc68a` |
| 24 | 1 | `6d64d638fe26069070fb465a4de2f3ae` |
| 24 | 2 | `5531b62ab748756b346ee02cbbf5d301` |
