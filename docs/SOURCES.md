# Sources and attribution

This project is derived from [Team-Haruki/Haruki-Sekai-Asset-Updater](https://github.com/Team-Haruki/Haruki-Sekai-Asset-Updater).
The service structure and engineering practices were adapted for BanG Dream! Our Notes;
unrelated Sekai business logic and upstream Git history are not distributed.
The Haruki MIT copyright and permission notice remain in [LICENSE](../LICENSE).

Game binaries, downloaded resources, real keys, account credentials and private research
are excluded from this repository and its release archives. Synthetic fixtures document
their origin. This project is not affiliated with or endorsed by the game's publishers.

The exporter uses [unity-rs](https://github.com/seiunx-dev/unity-rs) (`unity-rs-core` 0.5.1)
and [cridecoder](https://github.com/seiunx-dev/cridecoder) (0.3.5).
The legacy USM audio mask adapter is derived from cridecoder; its MIT notice is
retained in [LICENSE-cridecoder](../LICENSE-cridecoder).
[FFmpeg](https://ffmpeg.org/) is installed separately or through the Docker distribution's
package manager. It is not included in the standalone release archives.

Storage uses [Apache OpenDAL](https://opendal.apache.org/) 0.58.2 under Apache-2.0.
Its [license](../LICENSE-opendal) and [notice](../NOTICE-opendal) are included in archives and images.

The optional FFmpeg FFI codec bridge in `src/media_ffi*` is adapted from Haruki's generic
media layer at commit `3d33ed037f0ef5009e361e0535b3b19f8c239947` and retains its MIT attribution.
It uses MIT-licensed [rsmpeg](https://github.com/larksuite/rsmpeg) and separately installed
FFmpeg 7 libraries. See [development status and integration requirements](MEDIA_FFI.md).
