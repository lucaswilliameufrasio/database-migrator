#!/bin/sh

set -eu

cargo audit
cargo deny --all-features check
cargo cyclonedx --all-features --format json

python3 - <<'PY'
import json
from pathlib import Path

bom = json.loads(Path("bom.json").read_text(encoding="utf-8"))
if bom.get("bomFormat") != "CycloneDX":
    raise SystemExit("generated SBOM is not CycloneDX")
if not bom.get("components"):
    raise SystemExit("generated SBOM does not contain dependency components")
root_component = bom.get("metadata", {}).get("component", {})
if root_component.get("name") != "database-migrator":
    raise SystemExit("generated SBOM does not identify database-migrator")
PY
