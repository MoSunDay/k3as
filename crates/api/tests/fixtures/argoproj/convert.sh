#!/usr/bin/env bash
# Provenance: the JSON fixtures here are converted from the upstream Argo
# Workflows example YAMLs in link_repos/argo-workflows/examples (git f016ad89e).
# Re-run with: bash crates/api/tests/fixtures/argoproj/convert.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../../.." && pwd)"
SRC_DIR="$REPO_ROOT/link_repos/argo-workflows/examples"

# Representative single-document Workflow/CronWorkflow examples (T7.3 Scope A).
EXAMPLES=(
  hello-world                       # simplest container workflow
  steps                             # steps orchestration
  dag-diamond                       # DAG orchestration
  scripts-python                    # script template
  suspend-template                  # suspend template
  loops                             # withItems fan-out
  artifact-passing                  # input/output artifacts
  cron-workflow                     # CronWorkflow (single schedule)
  cron-workflow-multiple-schedules  # CronWorkflow (v4 schedules[])
  retry-backoff                     # retryStrategy + backoff + AnyString limit
  volumes-pvc                       # volumeClaimTemplates
)

python3 - "$SRC_DIR" "$SCRIPT_DIR" "${EXAMPLES[@]}" <<'PY'
import json, os, sys

import yaml

src_dir, out_dir = sys.argv[1], sys.argv[2]
examples = sys.argv[3:]

for name in examples:
    path = os.path.join(src_dir, f"{name}.yaml")
    with open(path, "r", encoding="utf-8") as fh:
        docs = [d for d in yaml.safe_load_all(fh) if d is not None]
    # Keep only the first Workflow/CronWorkflow document of the file.
    doc = next(
        (d for d in docs
         if d.get("kind") in ("Workflow", "CronWorkflow")
         and d.get("apiVersion") == "argoproj.io/v1alpha1"),
        None,
    )
    if doc is None:
        sys.exit(f"{name}.yaml: no argoproj.io/v1alpha1 Workflow/CronWorkflow doc")
    out = os.path.join(out_dir, f"{name}.json")
    with open(out, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    print(f"wrote {out} (kind={doc['kind']})")

PY
