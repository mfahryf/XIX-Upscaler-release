# FFmpeg Runtime Notice

XIX-Upscaler bundles the Windows x86-64 LGPL shared build of FFmpeg and
FFprobe so offline video processing does not depend on a system installation.

- Distributor: BtbN/FFmpeg-Builds
- Build release: `autobuild-2026-08-20-13-45`
- FFmpeg version: `n8.1.2-44-g7c533d0f86-20260820`
- Archive: `ffmpeg-n8.1.2-44-g7c533d0f86-win64-lgpl-shared-8.1.zip`
- Archive URL: https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-20-13-45/ffmpeg-n8.1.2-44-g7c533d0f86-win64-lgpl-shared-8.1.zip
- Archive SHA-256: `D311C8C7B86E06B54588E442652F963BAE165BD4D8393E73CC9EBB445B025547`
- Build scripts: https://github.com/BtbN/FFmpeg-Builds/tree/48576f1
- FFmpeg source revision: https://github.com/FFmpeg/FFmpeg/commit/7c533d0f86

The executable and shared-library files in `bin/` are copied without
modification from that archive. `ffplay.exe`, headers, import libraries,
presets, and generated HTML documentation are omitted because XIX-Upscaler
does not use them.

The package's GNU Lesser General Public License Version 3 text is included as
`LICENSE.txt`. The shared-library layout is retained so users can replace the
FFmpeg libraries with interface-compatible versions.
