#!/bin/bash
# Run QuickJS (native Rust), V8 (Node.js), and WASM benchmarks and compare results.
#
# Usage: ./benches/run_comparison.sh
#
# Options:
#   --skip-criterion    Skip Rust criterion benchmarks
#   --skip-v8           Skip V8/Node.js benchmarks
#   --skip-wasm         Skip WASM benchmarks

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RUST_DIR="$PROJECT_DIR/../.."

# Parse arguments
SKIP_CRITERION=false
SKIP_V8=false
SKIP_WASM=false

for arg in "$@"; do
  case $arg in
    --skip-criterion) SKIP_CRITERION=true ;;
    --skip-v8) SKIP_V8=true ;;
    --skip-wasm) SKIP_WASM=true ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

echo "=============================================="
echo "JavaScript Runtime Performance Comparison"
echo "=============================================="
echo ""
echo "Comparing:"
echo "  1. QuickJS (native Rust via FFI)"
echo "  2. V8 (Node.js native)"
echo "  3. QuickJS via amla-sandbox WASM"
echo ""
echo "=============================================="
echo ""

# Run Rust/QuickJS benchmarks
if [ "$SKIP_CRITERION" = false ]; then
  echo "Running QuickJS benchmarks (Rust/criterion)..."
  echo "----------------------------------------------"
  cd "$RUST_DIR"
  bench_out=$(cargo bench -p amla-js -- --noplot 2>&1)
  filtered=$(printf '%s\n' "$bench_out" | grep -E "(time:|thrpt:)" || true)
  printf '%s\n' "$filtered" | head -50
  echo ""
  echo ""
fi

# Run V8/Node.js benchmarks
if [ "$SKIP_V8" = false ]; then
  echo "Running V8 benchmarks (Node.js)..."
  echo "-----------------------------------"
  cd "$PROJECT_DIR"
  v8_out=$(node benches/compare_v8.mjs 2>&1)
  v8_filtered=$(printf '%s\n' "$v8_out" | grep -v "^$" || true)
  printf '%s\n' "$v8_filtered" | head -80
  echo ""
  echo ""
fi

# Run WASM benchmarks
if [ "$SKIP_WASM" = false ]; then
  echo "Running WASM benchmarks (amla-sandbox in Node.js)..."
  echo "-----------------------------------------------------"

  # Check if WASM binary exists, if not, build it
  WASM_PATH="$RUST_DIR/target/wasm32-wasip1/release/amla_sandbox.wasm"
  if [ ! -f "$WASM_PATH" ]; then
    echo "Building WASM binary..."
    cd "$RUST_DIR"
    cargo build -p amla-sandbox --target wasm32-wasip1 --release
  fi

  cd "$PROJECT_DIR"
  wasm_out=$(node benches/wasm_harness.mjs 2>&1)
  wasm_filtered=$(printf '%s\n' "$wasm_out" | grep -v "^$" || true)
  printf '%s\n' "$wasm_filtered" | head -80
  echo ""
  echo ""
fi

echo "=============================================="
echo "Comparison complete!"
echo ""
echo "For detailed criterion reports, see:"
echo "  target/criterion/report/index.html"
echo ""
echo "Key observations:"
echo "  - V8 has JIT compilation, QuickJS is interpreted"
echo "  - V8 excels at hot loops after warmup"
echo "  - QuickJS has lower memory overhead and faster startup"
echo "  - WASM adds ~2-5x overhead vs native QuickJS"
echo "  - WASM provides sandboxing and portability benefits"
echo "=============================================="
