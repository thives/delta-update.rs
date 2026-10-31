# Phase 8: adaptive mutation experiment

The adaptive policy was **rejected**, not enabled. It improved the tested 16 MiB image but regressed the 1 MiB image at 2 seconds and at a fixed 20 generations. The existing fixed weights remain in use. Phase 1d mutation counters and the corrected phase 1a diagnostic remain for further experiments.

## Method

On Intel Iris Xe Graphics (ADL GT2), compare a release binary built from the pre-phase-8 working tree with the experimental adaptive binary. The binaries were warmed, then run serially and interleaved, reversing order for seed 2. Inputs are the phase-0 `/tmp/bench/old1.img`, `new1.img`, `old16.img`, and `new16.img`. The CLI used the embedded profile, block size 4096, `-s 600 -m 0 -d auto`, and the repository's test signing key. Time budgets limit search, not upload, encoding, finalization, or process wall-clock. Fixed-generation runs used `-t 600 -g 20` and all reached 20 generations. All listed runs succeeded and selected Intel. Raw logs and payloads are in `/tmp/bench/phase8-runs/` for this session; they are temporary, not versioned artifacts. These measurements do not reproduce the HOST-profile historical reference points.

The candidate credited only the selected strictly improved global best, and used a bounded integer estimate of successes per proposal, including skips and refusals. Every eight mutation-bearing selections, a move with at least 32 pending samples updated a smoothed score with a prior of 1/128 and scaled its original weight, clamped to 2–64. This is an experimental policy, not a documented production algorithm.

## Paired results

Entries are payload bytes, baseline → adaptive, seeds 1, 2, 3 respectively. These are individual runs; seed and timing variation are not confidence intervals.

| Image / search budget | Seed 1 | Seed 2 | Seed 3 |
| --- | ---: | ---: | ---: |
| 1 MiB / 2 s | 198,145 → 201,916 | 173,180 → 173,068 | 199,801 → 209,039 |
| 1 MiB / 20 generations | 207,771 → 218,524 | 172,906 → 187,964 | 207,414 → 203,006 |
| 16 MiB / 2 s | 1,722,703 → 1,558,575 | 1,675,885 → 1,673,236 | 1,688,545 → 1,685,784 |
| 16 MiB / 20 generations | 1,751,487 → 1,571,853 | 1,678,862 → 1,672,317 | 1,684,911 → 1,673,775 |
| 16 MiB / 5 s | 1,688,072 → 1,541,196 | 1,673,420 → 1,632,487 | 1,663,585 → 1,597,766 |
| 16 MiB / 10 s | 1,703,339 → 1,540,855 | 1,676,590 → 1,638,415 | 1,655,105 → 1,614,768 |
| 16 MiB / 20 s | 1,733,979 → 1,531,757 | 1,660,394 → 1,628,557 | 1,653,804 → 1,597,187 |
| 16 MiB / 40 s | 1,721,605 → 1,546,290 | 1,663,111 → 1,614,258 | 1,659,527 → 1,593,505 |

The pinned small fixed-generation fixture also grew from 1,660 to 1,702 bytes. Because the 1 MiB gate failed, the candidate was withdrawn before conducting the remaining HOST and varied-workload performance comparisons; these results must not be interpreted as an exhaustive evaluation of adaptive weighting.

## Prerequisite measurements and validation

The original phase 1a forced-operator results are invalid: `evaluate` cleared the control words, so subsequent forced breeds saw an empty population and made immigrants. The corrected diagnostic reloads the population before **each** forced breed. On the 1 MiB image at 20 warm-up generations, crossover produced 231/1,152 admitted children beating the reloaded population's worst member; forced mutations had distinct skip and yield counts, rather than matching the immigrant totals. This is one seed and population, not a universal operator ranking.

The phase 1d instrumentation tags mutation children and counts proposals, skips, evaluation admissions, and strict global-best credits per move, saturating at `u32::MAX`. A selection credits at most one new best; seeds, immigrants and crossovers are excluded. `prepare`, `load_population`, and the destructive `evaluate` diagnostic reset the counters; archive restart preserves them. Fixed weights and RNG consumption are unchanged.

On the 1 MiB HOST diagnostic (seed 1), after 100 breeding generations, sweep had 48 new-best credits from 1,031 proposals, remove 17/344, copy-create 10/996, compact 9/497, and zero-create 4/356; all other moves had at most one. These are raw-fitness credits, not compressed-payload gains, and are workload-dependent.

The instrumented and reconstructed pre-instrumentation working trees matched all 36 full payload SHA-256 digests, lengths, raw fitnesses and candidate counts on Intel (nine workloads, HOST and EMBEDDED, seeds 1 and 2, 20 generations); all completed without refusals. Reconstructed baseline and instrumented logs are under `/tmp/zed-phase1d-matrix-verify/` for this session. The two package test suites passed on software and automatic/Intel selection.

The added 64 control words add 256 bytes to each control readback. A separate interleaved embedded 2-second check after withdrawing adaptation produced equal payloads for seeds 1 and 3 at both sizes, but seed 2 was 1,617 B larger at 1 MiB and 5,680 B larger at 16 MiB with instrumentation; the instrumented runs completed two and one fewer generations respectively. In a repeat, the 1 MiB seed-2 difference was 7,173 B, the 16 MiB seed-2 payloads matched, and the 16 MiB seed-3 instrumented payload was 63 B larger. These single-run time-budget differences are smaller than the observed seed ranges but the strict no-regression wall-clock gate is not demonstrated. The fixed-20-generation payloads matched for all six size/seed combinations. Logs are temporarily in `/tmp/bench/phase8-instrumentation-runs/` and `/tmp/bench/phase8-instrumentation-repeat/`.
