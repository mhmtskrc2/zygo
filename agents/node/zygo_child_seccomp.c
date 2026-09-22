/* zygo_child_seccomp — install ZYGO_CHILD_SECCOMP from a shared object's
 * constructor.
 *
 * A Node process cannot call `prctl`: there is no FFI in the standard library,
 * and the filter has to go in *after* the last `execve`, so a launcher that
 * installs it and then execs cannot work either — the filter it installs
 * denies the very `execve` it needs next.
 *
 * `process.dlopen` is the door Node does leave open. It runs a shared object's
 * constructors before it looks for an addon entry point, so everything here
 * happens and *then* Node throws "Module did not self-register", which the
 * agent expects and discards. There is no addon, no N-API and no Node header:
 * this object is loadable by any Node version, and by anything else with a
 * `dlopen`.
 *
 * Build it into your image next to the agent, or anywhere and point
 * `ZYGO_CHILD_SECCOMP_HELPER` at it:
 *
 *     cc -shared -fPIC -O2 -o zygo_child_seccomp.so zygo_child_seccomp.c
 *
 * Failure is loud and fatal. A worker that cannot install the filter must not
 * run the request: `spec/protocol.md` §3 says an agent that cannot install it
 * fails the request rather than running it unfiltered, and the surest way to
 * fail a request is to leave before it starts.
 */

#define _GNU_SOURCE
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

#define ENV "ZYGO_CHILD_SECCOMP"

/* Base64 decode in place-ish. Returns the byte count, or -1. */
static long unbase64(const char *in, unsigned char *out, size_t room) {
    static const char *alphabet =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    uint32_t accumulator = 0;
    int bits = 0;
    size_t written = 0;

    for (const char *p = in; *p; p++) {
        if (*p == '=') break;
        const char *at = strchr(alphabet, *p);
        if (at == NULL) return -1;
        accumulator = (accumulator << 6) | (uint32_t)(at - alphabet);
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            if (written >= room) return -1;
            out[written++] = (unsigned char)((accumulator >> bits) & 0xFF);
        }
    }
    return (long)written;
}

/* The kernel reads the instructions through the pointer at install time, so
 * the buffer has to outlive the call — and the program is small and bounded
 * (the `strict` child filter is a dozen instructions), so it lives here rather
 * than on a stack that is about to go. */
static unsigned char program[64 * 1024];

__attribute__((constructor)) static void zygo_install_child_filter(void) {
    const char *encoded = getenv(ENV);
    if (encoded == NULL || *encoded == '\0') return; /* nothing was asked for */

    long bytes = unbase64(encoded, program, sizeof(program));
    if (bytes <= 0 || bytes % (long)sizeof(struct sock_filter) != 0) {
        fprintf(stderr, "zygo: %s is not base64 of a seccomp program\n", ENV);
        _exit(121);
    }

    /* Already set by the launcher; harmless to repeat, and it is what lets an
     * unprivileged process install a filter at all. */
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
        perror("zygo: PR_SET_NO_NEW_PRIVS");
        _exit(121);
    }

    struct sock_fprog prog = {
        .len = (unsigned short)(bytes / (long)sizeof(struct sock_filter)),
        .filter = (struct sock_filter *)program,
    };
    /* Filters stack: the sandbox's own filter stays in force underneath. */
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog, 0, 0) != 0) {
        perror("zygo: PR_SET_SECCOMP");
        _exit(121);
    }
}
