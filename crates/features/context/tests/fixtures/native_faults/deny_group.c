#include <sys/types.h>
#include <signal.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
static int denied_killpg(pid_t pgid, int sig) {
    const char *inject = getenv("AAF_REVIEW_DENY_GROUP");
    if (inject && inject[0] == '1' && sig != 0) {
        fprintf(stderr, "REVIEW_INJECT killpg(%d,%d) -> EPERM\n", pgid, sig);
        errno = EPERM;
        return -1;
    }
    return killpg(pgid, sig);
}
__attribute__((used)) static struct { const void *replacement; const void *replacee; } interpose[]
__attribute__((section("__DATA,__interpose"))) = {{(const void *)denied_killpg, (const void *)killpg}};
