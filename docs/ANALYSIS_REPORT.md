# Local Telemetry Analysis Report (80 Sessions · 4,184 Turns)

This report documents the empirical measurements behind **AGY Hyper v0.4.x** — computed 100% locally from anonymized Antigravity transcripts using `agy-hyper audit --days 14 --sessions 80` and `agy-hyper bench --cache-ttl`.

> **Methodological honesty**: Every number below comes from real multi-day engineering sessions on a single workstation. Rather than projecting lab speedups onto your workload, AGY Hyper ships the exact same offline analyzer (`agy-hyper audit`) so you can verify your own numbers in ~10 seconds.

---

## 1. Where Quota & Latency Actually Go

| Metric | Measured Baseline (80 sessions · 4,184 turns) | What It Means |
| :--- | :---: | :--- |
| **Prompt Cache Hit Rate** | `89.7%` (`64` cold-cache breaks) | High average hit rate hides expensive prefix drops right after long pauses or background waits |
| **Uncached Input Share** | `38%` of API-equivalent spend | Even at ~90% token hit rate, uncached input + cache breaks still dominate dollar cost |
| **First-Edit Context Load** | `P50 = 151.4k` · `P90 = 241.3k` tokens | Half of coding sessions load >150k tokens *before editing a single line* |
| **Blanket-Read Turns** | `518` turns across `62` sessions | Repeated full-file exploratory reads keep rolling forward on every subsequent turn |
| **File Reads Share of Context** | `44.9%` of post-compact context | Almost half of working context consists of raw file reads rather than active edits or tests |

---

## 2. Empirical Per-Model Prompt Cache TTL Cliffs

Instead of assuming a single universal cache window across every model family, `agy-hyper bench --cache-ttl` and `agy-hyper audit` measure real cache-hit drop-offs by inter-turn idle gap (`delta_t`):

| Model Family | Sample Size (`n`) | Observed Cache Drop-off Cliff | Mode |
| :--- | :---: | :---: | :---: |
| `gemini-4-carbon` | `164` | **`~90s`** | Empirical |
| `claude-opus` | `420` | **`~120s`** | Empirical |
| `gemini-4-neon` | `2,943` | **`~180s`** | Empirical |
| `gemini-4-argon` | `1,226` | **`~180s`** | Empirical |
| `gemini-4-hydrogen` | `263` | `240s` | Default fallback (insufficient drop-off samples) |

**Practical takeaway**: A single slow build/test command that gets backgrounded past 10s and wakes the agent 2–3 minutes later—or switching models mid-session—routinely drops the prefix cache on `Carbon` (`90s`) and `Opus` (`120s`), forcing an uncached re-read of a `150k–240k` context window.

---

## 3. Observed Local Cohorts (Unadjusted)

| Cohort | Sessions | Turns | Turn Latency P50 | Prompt Cache Hit |
| :--- | :---: | :---: | :---: | :---: |
| **Native-only sessions** | `55` | `2,183` | `19.0s` | `91.1%` |
| **With-Hyper sessions** | `25` | `2,001` | `18.0s` | `90.9%` |

> **Why we report unadjusted numbers transparently**: With-Hyper sessions in this dataset skew toward heavier multi-file refactors (`80.0` turns/session vs. `39.7` turns/session in native-only sessions). Raw cross-session averages are confounded by task difficulty; `agy-hyper` never cherry-picks small subsets or hides sample sizes.

---

## 4. How AGY Hyper Addresses Each Bottleneck Without Breaking Native Workflows

1. **Zero-Intrusion First (`Observe` & `Safe` profiles)**:
   - `Observe` (`install.sh --observe`): live Hyper Deck cockpit, `agy-hyper audit`, passive WAL history capture — zero MCP tool interception, native diff cards 100% untouched.
   - `Safe`: enables fast local AST packing (`hyper_pack` / `hyper_search`) and synchronous checks (`hyper_check` up to 60s) while keeping native `edit_file` inline diffs untouched.
2. **Inline Line-Level WAL Diff & 1-Click Undo (`Balanced` / `Hyper`)**:
   - When full Rust atomic edits (`hyper_edit`) are enabled, bounded pre/post BLAKE3 snapshots render a color-coded Unified Diff drawer directly inside Hyper Deck (`agy-hyper tx diff`) with atomic 1-click Undo.
3. **Antigravity CLI (`agy`) First-Class Support**:
   - Verified end-to-end on `agy v1.5.14` (`agy plugin validate --strict` passes with 0 warnings; scans `~/.gemini/{antigravity,antigravity-ide,antigravity-cli}/brain/`), with `<2ms` native terminal statusline (`agy-hyper cli-setup --apply`) and standalone browser cockpit (`agy-hyper ui`).

---

## 5. Reproduce On Your Own Machine

```bash
# Install in zero-intrusion Observe mode first (never touches config.json)
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash -s -- --observe

# Run your own 14-day local burn audit and export a shareable 2x Retina PNG (or SVG) card
agy-hyper audit --days 14 --png my-audit.png
agy-hyper bench --cache-ttl
```
