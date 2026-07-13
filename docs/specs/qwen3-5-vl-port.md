# Spec: Qwen3.5-4B vision-language (VL) port

Status: in progress (branch `feature/qwen3-vl-port`)

## Goal

Run `Qwen/Qwen3.5-4B` (`Qwen3_5ForConditionalGeneration`) as an image-capable
model in rozum's native MLX runtime: given an image + a text prompt, produce a
text answer about the image. Today rozum's text backend **skips** VL models
(`crates/rozum-models` drops `Qwen3_5ForConditionalGeneration`), and the ported
`qwen3_5.rs` loads only the text stack (it drops `vision_tower.*` / `visual.*`
weights). This spec adds the vision path end-to-end.

Reference (vendored under `docs/`): transformers `modeling_qwen3_5.py` +
`vision_utils.py` (main branch). Weights: `mlx-community/Qwen3.5-4B-MLX-bf16`
(723 tensors; 297 `vision_tower.*`, text under `language_model.model.*`).

## Architecture (ground truth from config.json)

Text (`text_config`, model_type `qwen3_5_text`, already ported text-only):
- hidden 2560, 32 layers, 16 heads, 4 KV heads, head_dim 256, vocab 248320.
- Hybrid: GatedDeltaNet linear-attn on all but every `full_attention_interval`
  (=4)-th layer, which is full attention.
- rope_parameters: `mrope_interleaved: true`, `mrope_section: [11,11,10]`,
  `rope_type: default`, `rope_theta: 10_000_000`, `partial_rotary_factor: 0.25`
  → rotary dim = head_dim*0.25 = 64 → 32 freqs = 11+11+10. Text-only input →
  the 3 M-RoPE axes are equal → collapses to standard rope (why the text port
  works). **With images the 3 axes diverge — must implement M-RoPE.**

Vision (`vision_config`, model_type `qwen3_5`, Qwen3-VL ViT):
- depth 24, hidden 1024, 16 heads (head_dim 64), intermediate 4096,
  `gelu_pytorch_tanh` MLP, patch_size 16, spatial_merge_size 2,
  temporal_patch_size 2, in_channels 3, out_hidden_size 2560 (= text hidden),
  num_position_embeddings 2304 (→ num_grid_per_side = 48), deepstack indexes []
  (no deepstack). Full attention per image (no windowing).
- patch_embed: Conv3d(3, 1024, kernel=stride=[2,16,16]) — over flattened patch
  vectors of length 3*2*16*16 = 1536.
- pos_embed: learned Embedding(2304, 1024), bilinear-interpolated from the 48x48
  grid to the actual (h,w) grid (`get_vision_bilinear_indices_and_weights`).
- rotary: 2-axis (h,w) vision rope over head_dim//2=32 dims, positions from
  `get_vision_position_ids` (block-major over 2x2 merge blocks).
- merger: LayerNorm(1024) → Linear(1024*4, 1024*4) → GELU → Linear(4096, 2560).
  Merges each 2x2 spatial block → one 2560-dim token.

Special token ids: image 248056, video 248057, vision_start 248053,
vision_end 248054.

## Stages (each has a verification gate)

0. **Text base** — ✅ DONE. bf16 text stack loads + generates ("capital of France
   is Paris"). 426 params, `language_model.` prefix handled.
1. **Vision ViT** — ✅ DONE (`qwen3_5_vision.rs`, commit d8a7b03). Parity vs
   transformers `Qwen3_5VisionModel` on a 256² image: merged embeds cos=**0.9983**
   vs the f32 reference — tighter than torch's own bf16 path (cos 0.9964), since
   MLX accumulates bf16 matmuls in f32. Caught a real bug: the checkpoint stores
   the patch-embed conv weight channels-LAST `[out,T,ph,pw,C]`; must move C to
   position 1 before the Linear flatten. Probe: `examples/qwen3_5_vision_probe.rs`.
3. **Multimodal forward** — ✅ DONE (commit d2f1e6d). Splice + 3D interleaved
   M-RoPE via two thread-local hooks in `qwen3_5.rs` (text path byte-identical).
   End-to-end on the COCO two-cats image (grid 1×30×40) accurately describes
   "two tabby cats lying on a pink couch with two remotes". Example:
   `examples/qwen3_5_mm_generate.rs`. Uses oracle-produced pixel_values.
2. **Preprocess** — TODO. smart_resize + patchify + normalize (mean/std 0.5) →
   pixel_values [n_patches, 1536] + grid_thw, so rozum accepts raw images.
   Split: mlx-lm does smart_resize + patchify_normalize (pure math), rozum-mlx
   does image decode + resize (image crate).
4. **Gateway** — TODO. Push mlx-lm fork + bump rev; load vision tower alongside
   the text model; accept OpenAI `image_url` content; un-skip VL in the loader
   (`rozum-models/src/models.rs:398`, scan-time only).
5. **e2e + report** — partial (CLI e2e ✅). Gateway HTTP e2e + footprint pending.

## Non-goals (this pass)

- Video input (video_token path, timestamp separators).
- Deepstack (config has empty `deepstack_visual_indexes`).
- 4-bit/8-bit vision quantization (port against bf16 first).
