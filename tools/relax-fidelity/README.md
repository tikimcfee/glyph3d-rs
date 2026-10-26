# relax-fidelity: does relax compute what rustc computes?

`cargo glyph prove` under makepad's relax JIT only means something if relax
computes what the shipped rustc/LLVM release binary computes, and many checks
here are bit-exact on f32/f64. This directory holds the evidence, measured
2026-10-26 on an x86-64 host (Ryzen 9 9950X3D with AVX-512 and FMA, glibc
2.44, rustc 1.98.1) against relax at makepad `13595c1de`. It is its own cargo
workspace, outside every gate.

## Commands

```sh
tools/relax-fidelity/run.sh       # float harness: rustc vs relax (warm, cold, SSE)
tools/relax-fidelity/gates.sh     # the CPU instruments of reference-port, hyper-oracle, repo-verify
tools/relax-fidelity/goldens.sh   # every golden view, plain and under each equivalent
```

`gates.sh` and `goldens.sh` need `target/release/glyph3d-native` current
(`cargo glyph build`). The scripts run `relax` from PATH, or the binary named
in `$RELAX`. Relax runs the renderer as
`relax run --quiet --allow-c glyph3d-native -- <args>`. `--allow-c` is needed
because wayland-backend builds C. Its tests run as
`relax test --quiet --allow-c <package> [--lib|--tests]`. The renderer's first
cold compile under relax takes about three minutes; later runs reuse
`target/makepad-run`.

- `src/main.rs` runs float kernels over adversarial inputs (±0, subnormals, quiet
  and signalling NaNs with payloads, ties, cast edges, 2^k boundaries) and over
  random ones. Each result is printed as its bits, about 772k lines. Each
  operation is reached three ways: at run time through a function pointer on
  `black_box`ed inputs, on literals the optimiser may fold, and in `const`
  items. A 2^17-input sweep covers the transcendentals glyph3d calls, and
  glyph3d's own LUTs are included (the `powf(2.2)` tables, camera `sin_cos`,
  easing). The battery repeats `RELAX_FIDELITY_REPS` times (default 3) and
  every repetition is diffed against the first. Relax tiers code up while it
  runs (baseline, then O2, then O3), so a tier that computes differently shows
  up as a change between repetitions.
- `compare.py` sorts differences into NaN-payload-only (both results NaN) and
  value differences.

## What was run, and the result

| Check | rustc release | relax | Verdict |
|---|---|---|---|
| `--fixture-trie/-fold/-scan/-bake/-reference` (reference-port's five) | PASS | PASS, same stdout, same volumes (155,222 leaders / 1,874,328 lanes; 1,188,024 BIT + 53,752 within 1e-4; ...) | identical |
| `--hyper-oracle-check`, all four gate runs (leader, strict, `--tab-width 4`, `--tab-width 8`) | PASS | PASS, same stdout | identical |
| `--repo-verify`, hyper/direct × down/back | PASS | PASS, same stdout except throughput | identical |
| glyph3d-native lib tests (`hyper_oracle::tests::*`, `derived_vertex_stage_agrees_with_instanced`, `fu_to_world_is_the_nearest_f32`, fold/scan/bake, ...) | 297 ok | 297 ok, same per-test results | identical |
| 10 golden views × {plain, `--field-mode instanced`, `--field-mode visible`} | 30 BYTE-EQUAL | 30 BYTE-EQUAL to `baseline/vulkan-nvidia` | identical |
| glyph-field (10), -instanced (4), -derived (7), -visible (21), glyph-scene-graph (17), glyph (8), the renderer's 12 integration tests (`tab_stops`, `visible_verbs`, regions, wgsl, ...) | all ok | all ok, same per-test results | identical |

The fixture checks compare against the JS oracle's recorded answers, so
"PASS under both" there means both builds computed the oracle's bits. The two
tolerance tiers are the exception: fixture-scan's 53,752 within-1e-4 lanes, and
the lib tests' own tolerances. There, PASS under both does not prove the bits
match, only that both are inside the tolerance.

## Float semantics, operation by operation

From reading `libs/relax` (lowering in `compiler/src/lower.rs` `intrinsic`;
backends in `backend/src/{x64,arm64}.rs`; passes in `codegen/src/{opt,loops}.rs`
and `backend/src/passes.rs`; relax's own core in `compiler/std/core`) and from
the harness. "Identical" means bit-identical on every input the harness tried,
in every configuration: warm, cold cache with 16 repetitions, and
`MAKEPAD_RUN_NO_AVX=1` (the SSE encodings, with `mul_add` through the C `fma`).

| Operation class | Verdict | Notes |
|---|---|---|
| f32/f64 `+ - * /`, `%`, comparisons, `partial_cmp`, `total_cmp` | identical | scalar SSE/AVX at the operand's own width; no f32 work is done in f64 |
| `a*b + c` written out (all four operand orders) | identical | **no contraction**: relax makes `FMulAdd` only from the `fma` intrinsic (lower.rs) and `simd_fma` (lower/simd.rs). 6,000 triples chosen so that fused and unfused results must differ came out unfused |
| `mul_add` (f32, f64; negated operands) | identical values | relax core: `fma_native()` ? `vfmadd` : C `fma`/`fmaf`; rustc links compiler-builtins' `fmaf`/`fma`. All are correctly rounded |
| `sqrt` | identical | `sqrtss`/`sqrtsd` |
| identity folds `x*1`, `1*x`, `x/1`, `x-0.0`, `x+(-0.0)` (and `x+0.0`, which must not fold) | identical values | relax has the fold (`backend/src/passes.rs:1588-1607`, off under `MAKEPAD_RUN_NAN_EXACT`), but in the harness's closures it never fired on any tier: a signalling NaN came back quieted, which only the instruction does. rustc folds (LLVM instsimplify) and keeps the sNaN. So the difference is sNaN bits, in the opposite direction to the audit's reading. `x * -1.0`: rustc folds to a sign flip (`0xff800001`), relax multiplies (`0x7fc00001`). `x + 0.0` on `-0.0` gives `+0.0` on both, so neither folds it |
| `x / c` with `1/c` an exact power of two (2, 0.5, 2^±126, 2^127, 2^-149; f64 2^1022, 2^1023, 2^-1074) | identical | each beside the same division by a `black_box`ed constant, over the subnormal and NaN inputs too |
| `mul_add` at its edges: exact ties (13-bit operands, addend 0, ±tiny, ±half an ulp), products past `MAX` brought back by the addend, subnormal products | identical | 21,000 f32 and 21,000 f64 triples, under AVX (`vfmadd`) and under `MAKEPAD_RUN_NO_AVX=1` (C `fma`) |
| wasm-export FMA (the f32-via-f64 double rounding and the f64 Dekker emulation, `backend/src/wasm.rs`) | **not covered** | only the native JIT was run; a native prove never reaches the wasm backend |
| `std::simd` `mul_add` (`simd_fma`, emitted without a `cpu.fma` check) | **not covered** | `std::simd` is unstable on rustc 1.98, so there's no rustc side to compare; this host has FMA |
| `floor ceil trunc round round_ties_even fract` | identical values | relax inlines `roundss` (and its own `round`); rustc calls compiler-builtins. Only a signalling NaN differs: relax quiets it |
| `min`/`max`, both operands at run time | identical | including NaN handling |
| `CONST.min(x)` / `CONST.max(x)` with `x == CONST`, opposite sign of zero | **differs (zero sign)** | rustc returns the constant, whichever side it is written on (consistent with LLVM putting a commutative `minnum`'s constant on the right, where `minss` returns it on equality). Relax returns `x` (other) in every case. Rust leaves this unspecified. Repro below |
| `clamp`, `abs`, `copysign`, `signum`, `-x` | identical | |
| `powi` at run time | identical | relax: core's square-and-multiply, the same order as compiler-rt's `__powisf2` |
| `powi` on literals rustc constant-folds | differs from rustc's FOLD, equals rustc's RUN TIME | LLVM folds `powi` through `pow` in double, which is not the runtime algorithm: `0.1f32.powi(-2)` folds to `0x42c80000` and runs to `0x42c7ffff`. Relax gives `0x42c7ffff` both ways. rustc disagrees with itself here |
| `min`/`max`/`0/0` on literals rustc folds | NaN sign and zero sign only | LLVM folds `0.0/0.0` to `+qNaN`; x86 makes `-qNaN` (relax, and rustc at run time) |
| `as` casts f→int (every width, saturation, NaN→0) and int→f (u64→f32 double-rounding traps, i128/u128) | identical | |
| f32↔f64 conversion | identical except sNaN | LLVM deletes `(x as f64) as f32`, so rustc keeps a signalling NaN's bits; relax converts, which quiets it |
| `to_bits`/`from_bits`, moves through memory | identical | NaN payloads survive |
| NaN payload and sign through arithmetic | **differs** | which input NaN propagates when two are NaN (operand order), and quieting. Rust leaves NaN bits unspecified. Relax's own configurations differ here too: in a cold run a lane-accumulator loop's NaN payloads changed when its O3 build installed (the O3 payloads match rustc's), and the SSE build's payloads in that loop differ from the AVX build's |
| subnormals | identical | relax sets no FTZ/DAZ (MXCSR/FPCR is written only by an explicit `FpEnv`) |
| constant evaluation (`const` items) | identical | |
| sums, dot products, lane accumulators, element maps, scans (loops relax vectorises) | identical on finite data | the vectorisers keep per-lane order (`loops.rs`: "bit-identical"). Float reductions are never reassociated; only integer sums are |
| `sin cos tan sin_cos asin acos atan atan2 sinh cosh tanh exp exp2 exp_m1 ln log2 log10 ln_1p powf hypot`, f32 `cbrt` | identical (131k-input sweep and the specials) | both call the same glibc 2.44: rustc through the dynamic symbol, relax through `dlsym(RTLD_DEFAULT)` (relax core `libm.rs`, `compiler/src/os/proc.rs` `find`) |
| f64 `cbrt` | **differs, 1 ulp**: 182 of the 355 battery inputs, and every 1024-input sweep chunk | rustc statically links compiler-builtins' `cbrt` (CORE-MATH, correctly rounded: `nm` shows a local `t cbrt`); relax dlsyms glibc's, which is not. E.g. `cbrt(2^-1074)`: rustc `0x2990000000000000` (exact), relax `0x298fffffffffffff`. glyph3d calls no `cbrt` |
| 32-bit libm routing in general | **host-dependent** | rustc binaries carry compiler-builtins' `cbrt cbrtf ceil floor fma fmaf fmod round trunc rint`; relax takes those from the instruction or from the host's C library. On another libc or OS the list of differing functions can change |
| aarch64 / macOS, relax's interpreter tier | **not covered** | the interpreter runs only on aarch64 (`codegen/src/interp.rs` `maybe_compile`). The arm64 backend and Apple's libm were not measured. Run `run.sh` on the M2 before trusting an M2 prove |

### Where glyph3d meets the differences

- The CONST.min/max sign-of-zero shape. Only a literal RECEIVER differs:
  `x.min(CONST)` returns the constant on equality under both. The sweep
  `rg -n "[^a-zA-Z_.]-?[0-9]+\.[0-9]*(_?f(32|64))?\)?\.(min|max)\(" native/src crates glyph/src`
  finds only the safe form (`(… * 0.5).max(1.0)`, `pick.rs`'s `(…).max(0.0)`).
  The `.fold(0.0f32, f32::max)` reductions (deck.rs, layout_stack) put the
  literal in the receiver for the first element only, over `abs()` values or
  widths, so they would need a `-0.0` first element. Nothing on a layout path.
- NaN payloads: no gate compares NaN bits, and the layout produces no NaN on
  the corpora.
- `cbrt`: not called. `powi` on literals: only exact powers of two
  (`2f64.powi(-33)`), which every method gives exactly.

## Repros

```rust
// CONST.min(x): rustc 0.0, relax -0.0 (cargo run --release vs relax run --quiet)
use std::hint::black_box as bb;
#[inline(never)] fn c_min(x: f32) -> f32 { 0.0f32.min(x) }
fn main() { println!("{:?}", c_min(bb(-0.0))); }
```

```rust
// f64 cbrt: rustc 0x2990000000000000 (exact 2^-358), relax 0x298fffffffffffff
fn main() { println!("{:#x}", std::hint::black_box(f64::from_bits(1)).cbrt().to_bits()); }
```

Neither is a relax bug in the sense of violating Rust: both are behaviour Rust
leaves to the platform. They matter only to a bit-exact comparison against a
rustc-made record. If relax wanted rustc's bits, the fixes would be
`min_max_non_nan` / the `FBin(Min|Max)` lowering returning the constant
operand on equality, and routing `cbrt` (and the rest of compiler-builtins'
list) to a correctly-rounded implementation rather than the host libm.

## Reconciled with the makepad-side audit (2026-10-26)

agy's relax helper wrote its own reading of relax's float codegen
(`RELAX_PROVE.md` § 3 and its exceptions E1-E4; the copy in agent-mail predates
its § 3b, which carries this harness's findings). Each claim, against the
harness:

- No contraction; `mul_add` correctly rounded on both paths; compares, casts,
  `powi` order, no FTZ/DAZ, constant folding in the operation's own type:
  **confirmed** (rows above).
- Identity folds exact: **confirmed for values**. On the sNaN detail the
  harness shows the reverse of the static reading: rustc folds, relax (in this
  shape) does not (row above).
- `x/c -> x*(1/c)` only for an exact power of two: **confirmed** bit-exact
  against the unfolded division.
- E4, NaN payload/sign and the `min(-0.0, 0.0)` operand choice: **confirmed**.
  The zero-sign case is a literal receiver only.
- E1/E2 (wasm FMA) and E3 (`simd_fma` without a CPU check): **not tested**;
  outside a native prove.
- Missing from the audit's § 3, present in its § 3b: f64 `cbrt` 1 ulp,
  because rustc binaries carry compiler-builtins' libm subset and relax uses
  the host's. Its "transcendentals use the system libm, as rustc does" holds
  for every function except that subset.
- The audit's coverage gap (`mul_add` halfway and overflow cases) is now in
  the harness: identical.
