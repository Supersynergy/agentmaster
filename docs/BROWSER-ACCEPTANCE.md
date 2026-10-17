# Browser acceptance and benchmark receipts

The 2026-10-17 Office benchmark calibrated a false result with `ok:true` and
`verdict:false`. Transport success alone therefore cannot close a browser task.

Capture one complete JSON result from `mk act check`, retain its exit status,
then run the explicit filter on that file. Chain the capture and filter with
`&&` so a failed capture cannot reuse an earlier result. Use a fresh result
path per attempt. The filter requires `ok === true` and a boolean verdict equal
to the declared expectation. It rejects malformed JSON, missing fields, strings,
arrays, unreadable files and captures larger than 1 MiB. The default expectation
is true; `--expected false` supports deliberate negative assertions.

The successful receipt includes the absolute input path, SHA-256 of the exact
captured bytes and expected verdict. It does not validate the page assertion or
freshness by itself. Keep each assertion external to the agent's writable source
and record its hash before and after the run. A changed assertion invalidates
that comparison. Retain the coding source revision, dirty patch hash, fixtures,
model, effort, seed and parameters, tool versions and launcher targets alongside
the receipt. Record missing values as unknown.

Compare the same workload and acceptance in baseline and candidate runs; keep
cold and warm measurements separate. Account for retries, failed attempts,
checking and rework through integrated acceptance. Capture native provider
input, cache and output usage separately from elapsed time and output bytes.
Bytes divided by four are a size proxy; they are not measured model usage,
billing or savings. Browser operation microbenchmarks cannot establish whole
coding-task performance.

Deduplicate usage by canonical request and message IDs while preserving branch
identity; duplicate metric nodes must not count twice. Attribute usage to the
per-generation model metadata (`metadata.generation_model`), rather than the
session's current model. Record first update, first answer text and oracle
completion separately: `firstOutputMs` may include plan, tool or usage updates.
Missing usage remains unknown. Evidence: `DEViN-CONTROL-20261017.md`, SHA-256
`00acdaf68f29bd9100a0c66c09d8394bae71ef536c2663133464032901aeeee9`.

Source, build, installed launcher, live runtime and product acceptance are
separate evidence. This filter validates an assertion result; it does not prove
a live browser journey, a provider task or Office product acceptance.

Evidence inspected:
- `codex-office-agentbench-20261017/DEVIN-CHECK.md`, SHA-256
  `a793dbfad135a423be4738777da06be354c8542aebc46fab9aa881a291d863cd`.
- `codex-office-agentbench-20261017/raw/mk-verdict-calibration.json`, SHA-256
  `6648aa6367ae5d0a5c627beb59c8d255e7848d4537cdd57be4cbca3d3349d2eb`.
  This was a synthetic local boolean calibration, without model or Office
  acceptance.

Usage: `agentmaster browser-verdict /absolute/result.json --expected true`.
Use this command as the last step of a browser shell oracle supplied to
`goal --oracle` or `swarm --oracle`. Existing shell oracles retain their exit-code
contract; they do not automatically inspect JSON output.
