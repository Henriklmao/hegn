#!/usr/bin/env bash
# Measure line coverage for hegn's library unit tests.
#
# The script uses rustc's source-based coverage instrumentation and the system
# LLVM tools, so it does not require cargo-llvm-cov. The instrumented build
# uses a private target directory: it must not invalidate the normal build
# cache or race a parallel `cargo test`.
#
# Override the tools when the system LLVM does not match rustc's LLVM:
#   LLVM_COV=/path/to/llvm-cov LLVM_PROFDATA=/path/to/llvm-profdata scripts/coverage.sh
set -euo pipefail

cd "$(dirname "$0")/.."

LLVM_COV="${LLVM_COV:-llvm-cov}"
LLVM_PROFDATA="${LLVM_PROFDATA:-llvm-profdata}"

for tool in "$LLVM_COV" "$LLVM_PROFDATA" cargo rustc python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "coverage: missing required tool: $tool" >&2
    exit 1
  }
done

workdir="${TMPDIR:-/tmp}/hegn-coverage-$$"
trap 'rm -rf "$workdir"' EXIT
mkdir -p "$workdir/target"

export CARGO_TARGET_DIR="$workdir/target"
export RUSTFLAGS="${RUSTFLAGS:-} -C instrument-coverage"
export LLVM_PROFILE_FILE="$workdir/hegn-%p-%m.profraw"

echo "coverage: building instrumented library tests" >&2
binary="$(
  cargo test --lib --no-run --message-format=json 2>"$workdir/build.log" |
    python3 -c '
import json, sys
for line in sys.stdin:
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    if (
        message.get("reason") == "compiler-artifact"
        and message.get("executable")
        and message.get("profile", {}).get("test")
    ):
        print(message["executable"])
        break
'
)"
if [[ -z "$binary" ]]; then
  echo "coverage: could not locate the instrumented test binary" >&2
  sed -n '1,80p' "$workdir/build.log" >&2
  exit 1
fi

echo "coverage: running tests" >&2
"$binary" --test-threads=1 >"$workdir/test.log" 2>&1 || {
  cat "$workdir/test.log" >&2
  exit 1
}

shopt -s nullglob
profiles=("$workdir"/*.profraw)
if (( ${#profiles[@]} == 0 )); then
  echo "coverage: no LLVM profile data was produced" >&2
  exit 1
fi

echo "coverage: merging ${#profiles[@]} profile file(s)" >&2
"$LLVM_PROFDATA" merge -sparse "${profiles[@]}" -o "$workdir/hegn.profdata"

echo "coverage: report" >&2
"$LLVM_COV" report "$binary" \
  -instr-profile="$workdir/hegn.profdata" \
  --ignore-filename-regex='(^|/)(vendor|target|\.cargo)/' \
  --summary-only

echo
echo "Per-file coverage (hegn sources):" >&2
"$LLVM_COV" report "$binary" \
  -instr-profile="$workdir/hegn.profdata" \
  --ignore-filename-regex='(^|/)(vendor|target|\.cargo)/'
