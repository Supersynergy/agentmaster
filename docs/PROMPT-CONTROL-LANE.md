# Prompt control lane

AgentMaster treats local prompt-library files as untrusted text artifacts. The
catalog controls their provenance and delivery; it does not execute or follow
instructions found inside an artifact.

## Contract

- `prompts discover` requires one or more roots and can filter by local mtime
  day. It walks deterministically, skips symlinks and build/cache trees, admits
  only UTF-8 prompt/template/instruction-named text files of at most 128 KiB,
  and records canonical absolute paths with SHA-256 hashes.
- `prompts list` reads catalog metadata. `search`, `show`, `render`, and `send`
  re-hash the selected file before reading it; a changed or missing artifact
  fails closed.
- `render` is literal `{{key}}` substitution with bounded `--var key=value`
  inputs. It has no shell, tool, provider, or template-language evaluator.
- `send` needs both a registered non-duplicate ID and an explicit agent target.
  It calls AgentMaster's existing native PTY, cmux, or tmux sender; it never
  starts an agent or interprets artifact content as a controller command.
- `prompts doctor --json` validates schema, catalog hash, IDs, absolute paths,
  duplicate links, content hashes, regular-file status, size, and UTF-8.

## 2026-08-01 local inventory

The available local files are source reports, not automatically executable
templates. Discovery from `/Users/master/Downloads` registers:

| ID | role | SHA-256 |
| --- | --- | --- |
| `prompt-libraries-top50-2026-07-31` | catalog | `a3f01c6f474d95c180746e40abddaafdd6244018ca9e85887da9e424b08e3dfd` |
| `prompt-library-corpus-en` | corpus | `afe62afe1ff46c7664652f4d0cc753da24a938b23452d1809392e59aea29b2a0` |
| `prompt-library-corpus-en-plain` | duplicate | `11d6de56e96c135a6040028dec914305ef685c33441ae88322cce2772f1ba6d5` |

The plain corpus is a derivative and is intentionally excluded from normal
listing, rendering, and delivery.

## Curated source links

`docs/prompt-source-registry.v1.json` is compiled into AgentMaster and contains
45 direct URLs selected from the pinned Top-50 report: 13 prompt libraries, 16
skill libraries, 5 prompt tools, 7 system-prompt references, and 4 security
research sources. Agent applications with internal prompts, datasets, mirrors,
and generic survey lists are excluded.

```bash
agentmaster prompts links doctor --json
agentmaster prompts links list --json
agentmaster prompts links search "prompt optimization"
agentmaster prompts links show promptfoo --json
```

The link doctor parses the bundled registry, checks unique HTTPS IDs/URLs, then
requires every curated URL to occur in the registered source report after its
hash is rechecked. Links are not remote installs and cannot be passed to
`prompts send`; only separately registered local artifacts are deliverable.

## Operator oracle

```bash
agentmaster prompts discover --root /Users/master/Downloads --modified-on 2026-08-01 --json
agentmaster prompts doctor --json
agentmaster prompts list --all --json
agentmaster prompts search --json "red team"
agentmaster prompts render prompt-libraries-top50-2026-07-31 --json
agentmaster prompts send prompt-libraries-top50-2026-07-31 native:7 --dry-run
```

The expected completion condition is a healthy doctor report with three entries,
two active artifacts, and one duplicate. A real send remains an operator action;
the dry run proves target selection and no-write behavior.

## Independent Luna review

On 2026-08-02, `gpt-5.6-luna` reviewed the compact final contract through the
bounded LLMAdapter v2 lane. The provider reported 228 input and 98 output tokens
for prompt SHA-256 `f781c92e5b49a85110779ab5156b796d5189a451d297828907c6961ed8fd0295`.
Its exact verdict was `{"verdict":"pass","issues":[]}`. A prior response that
hit the output cap was not treated as a verdict.
