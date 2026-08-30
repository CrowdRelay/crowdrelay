#!/usr/bin/env python3
"""Source-level guard: prediction error must not cross into economic value.

Prediction error (fan_prediction_error, signal_prediction_error) is a
dopamine signal that updates beliefs/calibration. It MUST NOT directly
manufacture DecisionValue, economic value, portfolio value, or goal value.

This script enforces that `fan_prediction_error` and
`signal_prediction_error` are only referenced inside `causal_model.rs`
(where PredictionOutcome is defined and the update method lives). Any
reference outside that file is a semantic bypass and fails this guard.

The ONLY intended path is:
    PredictionOutcome (causal_model.rs)
    → model.update() updates beliefs
    → beliefs feed predict_stats_with_treatment()
    → TreatmentAwareStats feed DecisionValue

Prediction error must NOT appear in:
    - decision_value.rs
    - portfolio.rs
    - efe.rs
    - experiment.rs
    - evidence.rs
    - any infra/application/API file
"""
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
BRAIN_SRC = ROOT / "crates" / "crowdrelay-brain" / "src"

# These are the only files allowed to reference prediction error fields.
ALLOWED_FILES = {
    BRAIN_SRC / "causal_model.rs",
}

# These files must NEVER reference prediction error — they are the
# economic value surfaces where prediction error would manufacture
# artificial value if it leaked.
FORBIDDEN_PATTERNS = [
    "fan_prediction_error",
    "signal_prediction_error",
]


def find_rust_files() -> list[pathlib.Path]:
    """Find all .rs files in the workspace."""
    crates = ROOT / "crates"
    return list(crates.rglob("*.rs"))


def check_file(path: pathlib.Path) -> list[str]:
    """Check a single file for forbidden prediction error references."""
    violations = []
    try:
        content = path.read_text(encoding="utf-8")
    except Exception:
        return violations

    for pattern in FORBIDDEN_PATTERNS:
        if pattern in content:
            # Find the line number for the violation
            for i, line in enumerate(content.splitlines(), 1):
                if pattern in line:
                    violations.append(f"  {path}:{i}: {line.strip()}")
    return violations


def main() -> int:
    violations = []

    for rs_file in find_rust_files():
        # Skip the allowed files
        if rs_file in ALLOWED_FILES:
            continue
        file_violations = check_file(rs_file)
        violations.extend(file_violations)

    if violations:
        print("FAIL: prediction error references found outside causal_model.rs:")
        for v in violations:
            print(v)
        print()
        print("Prediction error (fan_prediction_error, signal_prediction_error)")
        print("must ONLY be referenced in causal_model.rs.")
        print("It must NOT cross into DecisionValue, portfolio value,")
        print("economic value, or goal value.")
        return 1

    print("OK: prediction error is isolated to causal_model.rs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
