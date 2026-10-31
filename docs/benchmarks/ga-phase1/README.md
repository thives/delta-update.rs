# GA phase 1: measure before changing behavior

**Correction:** The phase 1a forced-operator results and conclusions below are invalid. The diagnostic cleared the population control words with `evaluate` before breeding, so all forced modes actually produced immigrants. The diagnostic has since been corrected; see [phase 8](../ga-phase8/README.md) for the rerun and its limits. The repair-inheritance and plateau measurements are separate from this error.

Recorded 2026-10-30 from the current working tree. No shader or production GA source was changed; `tests/bench.rs` gained an ignored diagnostic only. Inputs are `/tmp/bench/old1.img`, `new1.img`, `old16.img`, and `new16.img` (the same phase-0 image set). The diagnostic converged for 20 generations, loaded the resulting population back into the session, and then tested 12 forced breeding generations per operator/mutation. Evaluated children were compared with the worst raw size in the reloaded population. The CLI runs used seed 1, `-m 0`, HOST resource limits (the values in `delta-update-payload/src/profile.rs`) unless noted, and the Intel Iris Xe Graphics (ADL GT2).

## 1a. Forced operator yield

Each forced mode proposed 1,152 children (12 generations × 96 children); none were skipped. All the modes and mutation IDs 1–16 gave the same aggregate counts within a given image size and device. No evaluated child beat the reloaded population's worst member.

| Image | Device | Valid | Refused | Refusal reasons | Beat worst kept |
| --- | --- | ---: | ---: | --- | ---: |
| 1 MiB | Intel Iris Xe | 771 | 381 | 381 invalid operation | 0 / 771 |
| 1 MiB | llvmpipe | 771 | 381 | 381 invalid operation | 0 / 771 |
| 16 MiB | Intel Iris Xe | 186 | 966 | 394 invalid operation; 572 extent capacity exhausted | 0 / 186 |
| 16 MiB | llvmpipe | 186 | 966 | 394 invalid operation; 572 extent capacity exhausted | 0 / 186 |

Refusal codes are mapped through `gpu_layout::STATUS_NAMES`. The population cap was 24 members in these sessions. Results say that, for these converged populations, none of these forced operators produced a child that could displace the worst kept member; the 16 MiB case also spends a majority of proposals on capacity/validity refusals. They do not establish operator yield over different seeds/workloads or explain which move is valuable early in search.

## 1b. Repair inheritance and dead data

The reloaded population had 24/24 members ending in a `Replace` operation; re-evaluating them required no additional repair (`mismatch_spans == 0` for all 24). A trailing `Replace` is only a repair-candidate proxy because the chromosome has no repair-origin tag; it is not proof that every such operation was appended by repair.

| Image | Trailing-Replace candidates | Repaired again | Fully overwritten raw data bytes / total data bytes |
| --- | ---: | ---: | ---: |
| 1 MiB | 24 / 24 | 0 / 24 | 23 / 2,369,250 (0.001%) |
| 16 MiB | 24 / 24 | 0 / 24 | 400 / 34,096,908 (0.001%) |

Dead-byte accounting unions later destination overlaps for each data operation, so bytes overwritten by multiple later operations are charged once. On this sample, inherited data deadness is negligible; these measurements do not support a repair-specific behavior change. The proxy limitation means exact repair-op counts would require a repair marker or tracing the shader's append path.

## 1c. Plateau stop and reference points

The CLI was run at `-t 5` and `-t 10` with the default 5-second stagnation window (`-s 5`), and again with `-s 600` to suppress plateau stopping. Each run stopped at the deadline, not stagnation. Thus the 5-second plateau rule fired 0/2 times in these runs. All values are payload bytes.

| Budget | Stagnation window | Stop | Generations | Search time | Payload | Historical reference |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 1 MiB / 5 s | 5 s | deadline | 32 | 4.962 s | 56,595 B | 31,889 B (historical Intel) |
| 1 MiB / 5 s | 600 s | deadline | 63 | 4.933 s | 36,833 B | 31,889 B (historical Intel) |
| 16 MiB / 10 s | 5 s | deadline | 41 | 9.085 s | 750,565 B | 489,271 B (historical) |
| 16 MiB / 10 s | 600 s | deadline | 53 | 9.710 s | 665,220 B | 489,271 B (historical) |

The longer stagnation window produced lower payloads here but still did not reproduce the historical values. The CLI search times exclude upload/encoding/finalization; these remain separate from full wall-clock. The 600-second setting was used only to prevent the plateau rule from truncating a time-budget run.

## Phase-1 gate decision

No GA behavior change is justified yet. Phase 1a provides enough evidence for the immediate hypotheses on the tested populations: immigrants/crossover/mutations did not improve on the kept population, while 16 MiB proposals are often refused. A second round with multiple seeds and workloads would be warranted before deciding whether per-move in-loop counters (phase 1d) are needed; they were not added in this phase. Repair-dead-byte measurements were approximately 0.001%, with the repair identification caveat above, so repair inheritance is not yet a demonstrated payload cost. The plateau rule did not fire in any of the four 5/10-second CLI runs, so these runs do not explain the remaining gap to the historical reference.

The phase-0 36-digest matrix remains the behavior-preservation reference. Since production/shader code was unchanged, no new digest matrix was generated. Post-change validation: `OTA_TEST_DEVICE=auto cargo test --release --workspace` passed. The first two parallel-thread software workspace runs failed only the timing-sensitive `stagnation_ends_the_search_before_the_clock_does` assertion (10.04 s and 10.42 s); that test passed alone, and `OTA_TEST_DEVICE=software cargo test --release --workspace -- --test-threads=1` passed. `cargo test --release -p delta-update-generator --test bench operator_yield_and_repair_inheritance --no-run` and diagnostics for `tests/bench.rs` passed.
