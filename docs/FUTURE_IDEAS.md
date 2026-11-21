# Future Ideas & Design Notes (`cardputer-zero-llrt`)

## Cortex-A53 NEON Vector Search (`cardputer:kv` extension — deferred)
**Status**: Deferred (valuable idea; do not build until a concrete consumer needs it).

### Key Hardware Trade-off on CardputerZero (512 MB RAM / ~80 MB free / Cortex-A53 1 GHz)
- **Search is cheap**: Brute-force cosine similarity over 5,000 × 384-dim vectors (int8-quantized ≈ 2 MB RAM, or f32 ≈ 7.5 MB RAM) takes **~1–3 ms** using ARMv8.0 NEON `vfmaq_f32` — no HNSW or external dependencies needed.
- **Embedding generation is the bottleneck**: Running a neural text embedding model locally on-device (`bge-small` / `e5-small` ≈ 40–80 MB quantized) consumes nearly all free RAM and risks zram/SD swap thrashing.

### When It Becomes Worth Building
1. **Remote-embedded offline RAG (ZeroClaw long-term memory)**: Generate embeddings on a host PC / API when writing memories, sync the vectors to CardputerZero, and search 5,000+ entries completely offline in ~1 ms.
2. **Model-free edge sensor classification**: Extract 32–128-dim feature vectors directly from `cardputer:imu` (gesture waveforms) or `cardputer:audio` (short-time spectrum) as `Float32Array` and run ~0.1 ms NEON k-NN classification on-device.

### Minimal Implementation Sketch (~150 lines in `cardputer:kv`)
- Avoid a separate crate; add `vecSet(key, Float32Array)` / `vecSearch(query, topK)` directly onto `cardputer:kv` so persistence piggybacks on `kv.flush()` atomic writes (~10 KB binary delta).
- For small text collections (< few hundred notes) or code/identifier lookup without an embedding pipeline, a zero-model BM25 / character 3-gram index is simpler and fully self-contained.
