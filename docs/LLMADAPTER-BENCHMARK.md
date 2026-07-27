# llmadapter payload benchmark

AgentMaster should send evidence, not raw bulk. This local projection payload
case study measures that principle without calling a model or provider. It is
not an equal-work, end-to-end agent A/B.

## Result

Measured on 2026-07-27 with 501 fresh bounded-payload builds:

| Metric | Result |
|---|---:|
| Synthetic log | 4,000 lines |
| Exact `ERROR` lines | 100 |
| `FINAL_NEEDLE` marker | line 3,777 |
| Naive inline payload | 184,497 bytes |
| Bounded capsule | 852 bytes |
| Payload reduction | **99.5382%** |
| Local build p50 | 0.7378 ms |
| Local build p95, nearest rank | 0.7883 ms |
| Provider calls | **0** |
| Self-check | **PASS** |

Fixture SHA-256:

```text
faf1ccfd29accad87e6c6f3ece2bc687e19af303c796a0f2c894ba95f4680884
```

The two payloads use the same objective and the same executable answer oracle.
Their preparation work intentionally differs:

- Naive: objective + oracle + all 4,000 log lines.
- Bounded: run a local projection, then package objective + oracle + projected
  evidence + artifact references.
- Declared case-study limits: up to three workers, a requested 500-token
  ceiling, and a 120-second deadline. Provider enforcement is not measured.

The capsule retains the exact local filter command. The self-check runs that
`awk` program and a separate Python projection, then requires both to return:

```json
{"error_count": 100, "marker_line": 3777}
```

It also writes that answer to a mode-`0600` file, exports the documented
`AGENTMASTER_ANSWER_PATH`, and executes the exact oracle string stored in both
payloads.

## Reproduce

Requires Python 3 and the system `awk`; no Python packages or provider
credentials.

```bash
cd /Users/master/projects/agentmaster
./scripts/benchmark_llmadapter_payload.py \
  --runs 501 \
  --fixture-out /tmp/agentmaster-llmadapter-fixture.log \
  --json-out /tmp/agentmaster-llmadapter-benchmark.json
```

The script prints the result JSON and optionally retains the fixture. Its
internal assertions cover fixture counts, marker position, filter result,
objective/oracle parity, the generated benchmark limits, answer-file mode, and
execution of the stored oracle. It does not inspect production AgentMaster
configuration.

## Claim boundary

This is a deterministic **local projection payload-capacity proxy**. It proves
that local filtering can remove 99.5382% of bytes from this constructed
workload while preserving the facts required by its oracle.

It does **not** prove 99.5382% lower provider-billed tokens, lower cost, better
