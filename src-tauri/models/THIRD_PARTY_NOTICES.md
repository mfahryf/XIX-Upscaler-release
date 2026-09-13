# Third-Party Model Notices

This directory contains model artifacts used by XIX-Upscaler. Exact file sizes,
SHA-256 hashes, tensor shapes, source repositories, and source commits are recorded
in `MODELS.json`.

## UpscalerJS ESRGAN Slim

- Project: UpscalerJS
- Repository: https://github.com/thekevinscott/UpscalerJS
- Pinned commit: `6d074fb5835a6c659a19c83123beab8c3f28beac`
- Selected source assets:
  - `models/esrgan-slim/models/x2/model.json`
  - `models/esrgan-slim/models/x2/group1-shard1of1.bin`
  - `models/esrgan-slim/models/x4/model.json`
  - `models/esrgan-slim/models/x4/group1-shard1of1.bin`
- Converted files: `esrgan-slim-x2.onnx`, `esrgan-slim-x4.onnx`
- Conversion command:
  `python tools/upscaler-models/build_esrgan.py --output XIX-Upscaler/src-tauri/models`

MIT License

Copyright (c) 2022 Kevin Scott

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Reproduction and Verification

Pinned source files and their hashes are in
`tools/upscaler-models/source-lock.json`. Reproduce and verify with:

```text
python tools/upscaler-models/build_esrgan.py --output XIX-Upscaler/src-tauri/models
python tools/upscaler-models/verify_models.py --family esrgan
python -m unittest discover -s tools/upscaler-models/tests -p "test_esrgan_conversion.py" -v
```
