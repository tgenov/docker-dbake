#!/usr/bin/env bash
# Benchmark: single-node docker buildx bake vs distributed docker dbake
#
# Usage: ./run.sh [num_services]
#   num_services: services to generate (default: 30)
#
# Prerequisites:
#   - docker dbake installed (cargo install --path ..)
#   - A multi-node buildx builder (set BUILDER env var, default: zot-lb)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NUM_SERVICES="${1:-30}"
BUILDER="${BUILDER:-zot-lb}"
RESULTS_FILE="$SCRIPT_DIR/results.csv"

# Initialize CSV if it doesn't exist
if [ ! -f "$RESULTS_FILE" ]; then
    echo "timestamp,services,nodes,test,seconds" > "$RESULTS_FILE"
fi

echo "═══════════════════════════════════════════════════════════"
echo " docker-dbake benchmark"
echo " Services: $NUM_SERVICES"
echo " Builder:  $BUILDER"
echo "═══════════════════════════════════════════════════════════"
echo ""

# --- Generate test services ---
echo "→ Generating $NUM_SERVICES services..."
bash "$SCRIPT_DIR/generate.sh" "$NUM_SERVICES" "$SCRIPT_DIR"
echo ""

# Discover nodes
NODE_LINES=$(docker buildx inspect "$BUILDER" 2>/dev/null | grep "^Name:" | tail -n +2)
NODE_COUNT=$(echo "$NODE_LINES" | wc -l | tr -d ' ')
ENDPOINTS=$(docker buildx inspect "$BUILDER" 2>/dev/null | grep "^Endpoint:" | awk '{print $2}')
FIRST_ENDPOINT=$(echo "$ENDPOINTS" | head -1)

echo "→ Builder '$BUILDER' has $NODE_COUNT nodes"
echo ""

TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

flush_cache() {
    echo "  (pruning build cache...)"
    docker buildx prune --builder "$BUILDER" -f --all >/dev/null 2>&1 || true
}

# ─────────────────────────────────────────────────────────
# Test 1: Single node — standard docker buildx bake
# This is the baseline. Bake sends all targets in ONE gRPC
# call to a single buildkitd which parallelizes internally.
# ─────────────────────────────────────────────────────────
echo "═══════════════════════════════════════════════════════════"
echo " Test 1: Single node (docker buildx bake)"
echo " Endpoint: $FIRST_ENDPOINT"
echo "═══════════════════════════════════════════════════════════"

SINGLE_BUILDER="bench-single-node"
docker buildx rm "$SINGLE_BUILDER" 2>/dev/null || true
docker buildx create --name "$SINGLE_BUILDER" --driver remote "$FIRST_ENDPOINT" >/dev/null
flush_cache

SINGLE_START=$(date +%s)
(cd "$SCRIPT_DIR" && docker buildx bake \
    --builder "$SINGLE_BUILDER" \
    -f docker-compose.yml \
    --progress plain 2>&1 | tail -3)
SINGLE_END=$(date +%s)
SINGLE_ELAPSED=$((SINGLE_END - SINGLE_START))

docker buildx rm "$SINGLE_BUILDER" 2>/dev/null || true
echo ""
echo "  ⏱  Single node: ${SINGLE_ELAPSED}s"
echo ""
echo "$TS,$NUM_SERVICES,1,single-node,$SINGLE_ELAPSED" >> "$RESULTS_FILE"

# ─────────────────────────────────────────────────────────
# Test 2: Distributed — docker dbake across all nodes
# dbake creates per-node shard builders and dispatches one
# target at a time per node via work-stealing.
# ─────────────────────────────────────────────────────────
echo "═══════════════════════════════════════════════════════════"
echo " Test 2: Distributed (docker dbake, $NODE_COUNT nodes)"
echo "═══════════════════════════════════════════════════════════"

flush_cache

DIST_START=$(date +%s)
(cd "$SCRIPT_DIR" && docker dbake \
    --builder "$BUILDER" \
    -f docker-compose.yml \
    --progress plain 2>&1 | grep -E '(^(✓|✗|---)|targets built|done.*failed)' | tail -10)
DIST_END=$(date +%s)
DIST_ELAPSED=$((DIST_END - DIST_START))

echo ""
echo "  ⏱  Distributed ($NODE_COUNT nodes): ${DIST_ELAPSED}s"
echo ""
echo "$TS,$NUM_SERVICES,$NODE_COUNT,distributed,$DIST_ELAPSED" >> "$RESULTS_FILE"

# ─────────────────────────────────────────────────────────
# Test 3: Sequential baseline — one target at a time
# Shows the theoretical worst case (no parallelism at all).
# Only runs if --sequential flag is passed.
# ─────────────────────────────────────────────────────────
if [[ "${2:-}" == "--sequential" ]]; then
    echo "═══════════════════════════════════════════════════════════"
    echo " Test 3: Sequential baseline (one at a time)"
    echo "═══════════════════════════════════════════════════════════"

    SEQ_BUILDER="bench-sequential"
    docker buildx rm "$SEQ_BUILDER" 2>/dev/null || true
    docker buildx create --name "$SEQ_BUILDER" --driver remote "$FIRST_ENDPOINT" >/dev/null
    flush_cache

    SEQ_START=$(date +%s)
    targets=$(cd "$SCRIPT_DIR" && docker buildx bake \
        --builder "$SEQ_BUILDER" \
        -f docker-compose.yml \
        --print 2>/dev/null | jq -r '.target | keys[]')

    for t in $targets; do
        echo "  building $t..."
        (cd "$SCRIPT_DIR" && docker buildx bake \
            --builder "$SEQ_BUILDER" \
            -f docker-compose.yml \
            --progress quiet \
            "$t" 2>&1 >/dev/null)
    done
    SEQ_END=$(date +%s)
    SEQ_ELAPSED=$((SEQ_END - SEQ_START))

    docker buildx rm "$SEQ_BUILDER" 2>/dev/null || true
    echo ""
    echo "  ⏱  Sequential: ${SEQ_ELAPSED}s"
    echo ""
    echo "$TS,$NUM_SERVICES,1,sequential,$SEQ_ELAPSED" >> "$RESULTS_FILE"
fi

# ─────────────────────────────────────────────────────────
# Results
# ─────────────────────────────────────────────────────────

if [ "$SINGLE_ELAPSED" -gt 0 ] && [ "$DIST_ELAPSED" -gt 0 ]; then
    SPEEDUP=$(echo "scale=2; $SINGLE_ELAPSED / $DIST_ELAPSED" | bc 2>/dev/null || echo "N/A")
    if [ "$(echo "$SPEEDUP > 1" | bc 2>/dev/null)" = "1" ]; then
        VERDICT="dbake is ${SPEEDUP}x faster"
    else
        INVERSE=$(echo "scale=2; $DIST_ELAPSED / $SINGLE_ELAPSED" | bc 2>/dev/null || echo "N/A")
        VERDICT="single node is ${INVERSE}x faster (builds not heavy enough to saturate one node)"
    fi
else
    VERDICT="N/A"
fi

echo "═══════════════════════════════════════════════════════════"
echo " Results: $NUM_SERVICES services"
echo "═══════════════════════════════════════════════════════════"
echo ""
echo "  Single node (1 buildkitd):     ${SINGLE_ELAPSED}s"
echo "  Distributed ($NODE_COUNT nodes):         ${DIST_ELAPSED}s"
echo ""
echo "  Verdict: $VERDICT"
echo ""
echo "  Note: single-node bake sends all targets in one gRPC call;"
echo "  buildkitd parallelizes them internally. dbake's advantage"
echo "  appears when builds are heavy enough to saturate a single"
echo "  node's CPU/RAM, or when platform constraints require"
echo "  different architectures."
echo ""
echo "═══════════════════════════════════════════════════════════"
echo "Results saved to $RESULTS_FILE"
