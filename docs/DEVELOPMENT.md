# Development Notes

## AI model guidance per milestone

Mudular is built with Claude Code. Model choice per session materially affects
cost; the roadmap's milestones (ARCHITECTURE.md §14) differ enough in
difficulty to justify matching the model to the work. Default cheap, escalate
on evidence.

| Milestone | Work | Model |
|---|---|---|
| M0 Walking skeleton | Wiring, TUI shell, straightforward async plumbing | Sonnet |
| M1 Proper Telnet | Option state machine (RFC 1143), prompt semantics — correctness-critical | Opus |
| M2 STelnet | TLS plumbing, cert pinning — well-trodden patterns | Sonnet |
| M3 Config & profiles | serde schemas, directory discovery, CLI | Sonnet |
| M4 Automation engine | Scope merge semantics + regex plumbing | Sonnet; Opus if merge/shadowing logic gets subtle |
| M5 MCCP | Mid-buffer compression switchover — subtle stream-state bugs | Opus |
| M6 GMCP + MSDP | Well-specified codecs with byte fixtures | Sonnet |
| M7 Multi-character | Concurrency, channel wiring, focus/layout, cross-session routing | Opus |
| M8 Scripting | ScriptHost abstraction, FFI bindings, sandboxing | Opus |
| M9 Polish | Independent small features | Sonnet |

Rules of thumb:

- **Default: Sonnet** — the TAD makes most milestones well-specified, which is
  exactly where the cheaper tier performs at near-Opus quality.
- **Opus** for milestones marked above, and whenever a session is mostly
  *debugging byte streams or concurrency* rather than writing new code.
- **Escalate on evidence**: if the current model has failed the same problem
  twice, move up one tier with a fresh session rather than iterating in place.
- **Fable**: not budgeted for scheduled work. Escalation-only — a design
  review of a cross-cutting change, or a bug that survived an Opus session.
  Note: Fable carries extra security classifiers; raw-socket/Telnet work is
  benign but can rarely trip a false-positive refusal — switch that session
  back to Opus instead of rephrasing.
- **Haiku** only as a subagent for searches, never for protocol code.
- Give whichever model the full task spec up front (link the TAD section and
  the milestone's "done when" criterion) — specificity is what lets the
  cheaper tiers punch up.
