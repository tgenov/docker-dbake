#!/usr/bin/env bash
# Generate a synthetic docker-compose.yml with N services of varying build times.
# Each service installs packages and compiles code, creating realistic CPU-bound builds
# that saturate a single node when run concurrently.
#
# Usage: ./generate.sh [num_services] [output_dir]
#   num_services: number of services to generate (default: 30)
#   output_dir:   directory for Dockerfiles (default: .)

set -euo pipefail

NUM_SERVICES="${1:-30}"
OUTPUT_DIR="${2:-.}"
SERVICES_DIR="$OUTPUT_DIR/services"

mkdir -p "$SERVICES_DIR"

# Tier determines build complexity — these are heavy enough to saturate
# a single node when 30+ run concurrently:
#   light:  compile + run prime sieve to 5M (~15s solo, much slower under contention)
#   medium: compile + run to 15M (~40s solo)
#   heavy:  compile + run to 30M (~80s solo)
#   huge:   compile + run to 60M (~160s solo)
# Distribution: 40% light, 30% medium, 20% heavy, 10% huge
TIERS=(light light light light medium medium medium heavy heavy huge)

make_dockerfile() {
    local name="$1"
    local tier="$2"
    local file="$3"

    case "$tier" in
        light)
            cat > "$file" << EOF
FROM alpine:3.21
RUN apk add --no-cache build-base linux-headers
COPY services/workload.c /tmp/workload.c
RUN gcc -O2 -o /tmp/workload /tmp/workload.c -lm \\
    && /tmp/workload 5000000
EOF
            ;;
        medium)
            cat > "$file" << EOF
FROM alpine:3.21
RUN apk add --no-cache build-base linux-headers
COPY services/workload.c /tmp/workload.c
RUN gcc -O2 -o /tmp/workload /tmp/workload.c -lm \\
    && /tmp/workload 15000000
EOF
            ;;
        heavy)
            cat > "$file" << EOF
FROM alpine:3.21
RUN apk add --no-cache build-base linux-headers
COPY services/workload.c /tmp/workload.c
RUN gcc -O2 -o /tmp/workload /tmp/workload.c -lm \\
    && /tmp/workload 30000000
EOF
            ;;
        huge)
            cat > "$file" << EOF
FROM alpine:3.21
RUN apk add --no-cache build-base linux-headers
COPY services/workload.c /tmp/workload.c
RUN gcc -O2 -o /tmp/workload /tmp/workload.c -lm \\
    && /tmp/workload 60000000
EOF
            ;;
    esac
}

# Create a CPU-bound workload that actually saturates a core
cat > "$SERVICES_DIR/workload.c" << 'WORKLOAD'
#include <stdio.h>
#include <stdlib.h>
#include <math.h>

/* CPU-bound workload: compute primes via trial division.
 * This genuinely saturates a CPU core — unlike sha256sum piped from seq,
 * which is I/O bound on the pipe. */
int main(int argc, char *argv[]) {
    long limit = argc > 1 ? atol(argv[1]) : 1000000;
    long count = 0;
    for (long n = 2; n < limit; n++) {
        int is_prime = 1;
        for (long d = 2; d * d <= n; d++) {
            if (n % d == 0) { is_prime = 0; break; }
        }
        count += is_prime;
    }
    printf("Found %ld primes below %ld\n", count, limit);
    return 0;
}
WORKLOAD

cat > "$OUTPUT_DIR/docker-compose.yml" << 'HEADER'
# Auto-generated benchmark compose file.
# Each service does real CPU work (prime computation + package installs)
# to simulate builds that saturate node resources.
services:
HEADER

light=0 medium=0 heavy=0 huge=0

for i in $(seq 1 "$NUM_SERVICES"); do
    name="svc-$(printf '%03d' "$i")"
    tier_idx=$(( (i - 1) % ${#TIERS[@]} ))
    tier=${TIERS[$tier_idx]}

    case "$tier" in
        light)  light=$((light + 1))  ;;
        medium) medium=$((medium + 1)) ;;
        heavy)  heavy=$((heavy + 1))  ;;
        huge)   huge=$((huge + 1))   ;;
    esac

    make_dockerfile "$name" "$tier" "$SERVICES_DIR/Dockerfile.$name"

    cat >> "$OUTPUT_DIR/docker-compose.yml" << EOF
  $name:
    build:
      context: .
      dockerfile: services/Dockerfile.$name
    image: benchmark/$name:latest
EOF
done

echo "Generated $NUM_SERVICES services in $OUTPUT_DIR"
echo "Tier distribution:"
echo "  Light  (~15s):  $light"
echo "  Medium (~40s):  $medium"
echo "  Heavy  (~80s):  $heavy"
echo "  Huge  (~160s):  $huge"
echo ""
echo "Estimated sequential time: ~$((light*15 + medium*40 + heavy*80 + huge*160))s"
