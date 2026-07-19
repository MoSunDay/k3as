# TODO template

Every TODO in `index.md` and `plan/*.md` MUST provide these **7 fields**, in
this order, using this exact heading vocabulary (English heading + Chinese
gloss is the house style; do not rename the headings):

---

## <TODO-ID> — <short title>

- **目标 / Goal**
  One sentence describing the outcome. Verifiable, not aspirational.

- **核心实现 / Core implementation**
  The concrete approach: crates/modules touched, key algorithms, external
  deps (with versions), and how it satisfies the locked decisions (Q1–Q5).
  Reference `link_repos/...` paths where we mirror upstream behavior.

- **验收手段 / Acceptance**
  An executable, reproducible check. Prefer:
  1. an automated test in `cargo test`, or
  2. a scripted scenario invoking a real upstream client (`kubectl`,
     `helm`, `kube-rs` example), or
  3. a conformance golden-case from **T0.6**.
  "Code review" is never sufficient on its own.

- **状态 / Status**
  One of: `not-started` · `in-progress` · `blocked` · `done`.
  (All ship `not-started`; T0.5 ships `in-progress`.)

- **证据 / Evidence**
  When `done`: links/paths to tests, artifacts, command transcripts.
  While `not-started`/`in-progress`: leave `—` or a TODO pointer.

- **卡点 / Blockers**
  Open risks, unknowns, decisions still needed. `none` if clear.

- **依赖 / Depends on**
  Other TODO IDs that must be `done` first. Drives the DAG in `index.md`.

---

### Style rules

- TODO IDs are globally unique: `<layer-prefix><n>` (e.g. `T5.4`).
- IDs are **identical** between `index.md` and the corresponding
  `plan/<layer>.md` — edit both together.
- Keep each TODO under ~40 lines; link out to design notes if longer.
- No status changes without updating `index.md` status table + `证据`.
