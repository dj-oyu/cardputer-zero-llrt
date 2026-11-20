# CardputerZero LLRT — Optimization Discoveries Log (7-Tier Scale)

Record every meaningful optimization attempt here — including **Dead Ends** (things tried that did not help or regressed), because negative results prevent repeating failed experiments.

## 7-Tier Rating Scale (Strict — Reserve Top Tiers for True Breakthroughs)

| Tier | Name | Criteria |
|---|---|---|
| **7** | **Epic** | Paradigm shift / structural breakthrough (≥ 2.0× on a major workload or ≥ 20% total real-device speedup across subsystems) |
| **6** | **Nobel Prize** | Architecture-level discovery (15–30% real-device win on a key workload, or breaking a fundamental FFI/memory/SIMD barrier) |
| **5** | **Genius** | Sharp Cortex-A53 hardware insight (in-order pipeline, L1 32KB, NEON 16B) or algorithmic win (5–15% win) |
| **4** | **Insightful** | Solid, reproducible medium optimization (2–5% win with zero regressions) |
| **3** | **Useful** | Clean small improvement (0.5–2% win or measurable allocation/binary reduction) |
| **2** | **Note** | Measurement observation, hardware quirk, profiler finding, or variance analysis |
| **1** | **Dead End** | Tried and failed / neutral / regressed — **always log why so we never repeat it** |

---

## Entry Template

```markdown
### [TierName] Short descriptive title (YYYY-MM-DD HH:MM)
- **Hypothesis**: What bottleneck we expected to remove.
- **Change**: Exact files/functions modified.
- **Host x86_64**: before -> after (% delta)
- **Real CardputerZero A53**: before -> after (% delta, LSE/RCpc status)
- **Takeaway / Sweep Potential**: Where else in `crates/*` this pattern might apply.
```

---

## Discoveries

_(Entries will be appended below during the Final Boss optimization campaign.)_
