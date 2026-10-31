# GA phase 0 baseline

Recorded 2026-10-30 from HEAD `e5b420c23662b56cdc9aa478f80cd50cbf7dd250`, with no GA source changes in the working tree. HEAD was extracted with `git archive`; the HEAD and working-tree release binaries were built with separate `CARGO_TARGET_DIR`s. Runs were serial and interleaved HEAD/work, reversing their order for seed 2. `auto` selected Intel Iris Xe Graphics (ADL GT2); `software` selected llvmpipe (LLVM 21.1.8, 256 bits).

## Artifacts

- `matrix.txt`: 36 HEAD matrix rows, seeds 1 and 2, 20 generations, nine workloads and both HOST and EMBEDDED profiles. The digest is the first eight bytes of SHA-256 of the complete payload. All rows reached 20 generations; no build was refused. This is the behavior-preservation reference.
- `host-runs.jsonl` and `runs.jsonl`: 48 HOST and 48 EMBEDDED CLI runs respectively. Each JSON record includes the full CLI output, including payload, generations, candidates, upload/search/encoding/finalization times and stop reason. Each combination of 1/16 MiB, `-t 2/5/10` or `-g 20`, and seeds 1/2/3 has both HEAD and working-tree runs. All runs succeeded and generation-capped runs reached 20 generations. The generation cap was accompanied by `-t 600`; all runs used `-s 600 -m 0 -d auto`. `-t` is the search budget, not total wall-clock time; encoding and finalization follow it. `elapsed_s` is process wall-clock time.
- `profile-{auto,software}-{1,16}.txt`: independent HOST-profile session measurements with `SearchConfig::quick()` (seed 1), no warm-up, 12 individually submitted profiled generations and a 20-generation batch. They include per-stage mean/worst times and refusal counters after seeding. Counter index 2 is `S_STRUCT`, index 5 is `S_EXTCAP`; other indices were zero. These profiles are not per-run stage timings or final search refusal counts.
- `tests-{auto,software}.txt`: release workspace correctness test output on the two device selections; both commands passed. Ignored benchmarks are not part of these test runs.

Inputs were `/tmp/bench/old1.img`, `new1.img`, `old16.img`, and `new16.img`, respectively SHA-256:

```text
d440fd350fec5390308e2f3bb3f11c640066338d3d8911b43f8edc7c6070b666
46324592f5cdf3051757ef72434edfdbd5cf04b12aced4aef81cf581423fef6a
632309dc8ac94e1783fb2c60af09f57e961b867370c05c3fb2b720afaee9cdc0
c49039d28698613092047f415ca8d875a18635d43237cf86e111aa88889c7e1e
```

The CLI used `delta-update-payload/tests/fixtures/private.pem`, block size 4096 and partition `A/B`. The HOST runs set all resource-limit CLI flags to the values in `delta-update-payload/src/profile.rs` (`HOST`); the other runs used CLI defaults (`EMBEDDED`). Matrix command: `OTA_MATRIX_SEEDS=1,2 OTA_MATRIX_GENERATIONS=20 OTA_TEST_DEVICE=auto cargo test --release -p delta-update-generator --test bench workload_matrix -- --ignored --nocapture`.

## HEAD HOST results on Intel

Entries are payload bytes for seeds 1, 2, 3, respectively; these ranges are a crude seed-spread noise reference, not a statistical confidence interval.

| Image | Budget | Payload bytes | Seed range |
| --- | --- | --- | --- |
| 1 MiB | 2 s | 63,607; 67,781; 75,374 | 11,767 |
| 1 MiB | 5 s | 37,129; 41,068; 49,675 | 12,546 |
| 1 MiB | 10 s | 28,725; 32,899; 42,353 | 13,628 |
| 1 MiB | 20 generations | 71,637; 71,060; 84,738 | 13,678 |
| 16 MiB | 2 s | 1,238,717; 1,167,285; 1,250,836 | 83,551 |
| 16 MiB | 5 s | 885,285; 826,038; 861,988 | 59,247 |
| 16 MiB | 10 s | 657,034; 571,137; 584,593 | 85,897 |
| 16 MiB | 20 generations | 1,002,654; 924,360; 989,713 | 78,294 |

The historical §12 figures (31,889 B at 1 MiB/5 s and 489,271 B at 16 MiB/10 s) were not reproduced; HEAD seed 1 measured 37,129 B and 657,034 B, respectively. The measured generation counts were 62 and 55, versus 88 and 92 in §12. Differences in runtime conditions and/or inputs may contribute; their cause was not established. At fixed 20 generations HEAD and working-tree payloads and candidate counts matched for all seeds and sizes in both profiles.

## Limitations

The existing CLI does not report per-reason GA refusals or individual GPU stage timings for each time- and generation-budget run. The available profiling test supplies those stage timings and seed-time refusals only for a separate seed-1 HOST session; it does not provide the requested per-budget/per-seed refusal and stage matrix. Capturing that matrix requires additional instrumentation, contrary to phase 0's no-code constraint. No claim about such per-run values is made. The time-budget runs were performed on Intel; software was covered by the independent stage profiles and correctness suite, not the paired budget matrix.
