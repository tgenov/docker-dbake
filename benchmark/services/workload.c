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
