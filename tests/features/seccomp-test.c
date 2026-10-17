/* Installs an allow-all seccomp filter (no_new_privs first, as an
 * unprivileged container runtime does) and checks /proc/self/status says
 * filter mode. */
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>

int main(void) {
    struct sock_filter filter[] = {BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW)};
    struct sock_fprog program = {.len = 1, .filter = filter};
    char line[256];
    int mode_two = 0;
    FILE *status;

    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
        prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) != 0) {
        perror("prctl");
        return 1;
    }
    status = fopen("/proc/self/status", "r");
    if (status == NULL) {
        perror("/proc/self/status");
        return 1;
    }
    while (fgets(line, sizeof line, status) != NULL) {
        if (strncmp(line, "Seccomp:", 8) == 0) {
            fputs(line, stdout);
            mode_two = strstr(line, "2") != NULL;
        }
    }
    fclose(status);
    return mode_two ? 0 : 1;
}
