# Performance fixtures

`upstream/` contains the five benchmark inputs used by the TypeScript comparator. Four files are extracted byte-for-byte from the digest-verified `microsoft/TypeScript` archive at suite commit `4d4f005c8541e0255a9d8791205fdce326e462bc`; `empty.ts` is the zero-byte `filefixture.FromString("empty.ts", "")` input declared by `microsoft/typescript-go` at compiler commit `2bd066d87f5bafd315be9f40889d0a60b9e58e0b`.

Regenerate the tracked upstream fixtures and ignored boundary inputs from the repository root:

```text
perf_budget materialize-fixtures --manifest perf/benchmarks.toml
perf_budget verify-fixtures --manifest perf/benchmarks.toml
```

`boundary/` is generated and ignored. In particular, the three approximately 16 MiB `source-bytes` fixtures must not be committed. Corpus fixture rows reference the validated trees under `corpus/projects/`; they are not copied here.

A baseline is valid only while the live process and machine match `perf/hosts/bh1.toml` exactly. Schema-v2 artifacts record governor, `SwapTotal`, CPU affinity, NUMA policy, and memory-node mask. `bless-baseline`, `capture-scorecard`, `check-baseline`, `check-scorecard`, and `compare` reject drift. Run them under `numactl --physcpubind=0-19 --membind=0`; never hand-create or edit an artifact to bypass the gate.

The TypeScript scorecard runs each upstream single-file `BenchFixtures` input with `--noEmit --pretty false --allowJs --jsx preserve`. These parser/binder fixtures can produce diagnostics without an infrastructure failure. Schema v2 therefore records each pinned `tsc` exit code and rejects exit drift or signal termination.
