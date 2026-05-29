/*
 * wslptyd — a multiplexed PTY *server* that runs once inside a WSL distro and
 * manages many PTY sessions over a single stdin/stdout connection. This lets a
 * terminal app open N windows with just ONE wsl.exe + ONE server process (plus
 * the N shells), instead of one wsl.exe + one helper per window.
 *
 * Wire protocol (both directions): length-prefixed frames, little-endian:
 *     [u32 session][u8 type][u32 len][payload...]
 *
 *   host -> server:
 *     1 OPEN   : [u16 cols][u16 rows][u32 cwdLen][cwd][u32 shellLen][shell]
 *     2 DATA   : raw bytes -> write to that session's pty master
 *     3 RESIZE : [u16 cols][u16 rows] -> TIOCSWINSZ
 *     4 SIGNAL : [u8 signo] -> kill(child, signo)
 *     5 CLOSE  : (empty) -> SIGHUP + reap the session
 *   server -> host:
 *     2 DATA   : raw pty output for that session
 *     6 EXIT   : [u32 exitcode] -> session ended
 *
 * Each OPEN does a real forkpty() -> /dev/pts/N with the login shell, exactly
 * like the single-session helper, so every session is a genuine TTY.
 */
#define _GNU_SOURCE
#include <pty.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <signal.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <fcntl.h>
#include <stdint.h>

#define MAX_SESS 256

typedef struct {
    uint32_t id;
    int      master;
    pid_t    pid;
    int      active;
} Sess;

static Sess g_sess[MAX_SESS];

static ssize_t writen(int fd, const void *buf, size_t n) {
    const char *p = (const char *)buf;
    size_t left = n;
    while (left) {
        ssize_t w = write(fd, p, left);
        if (w < 0) { if (errno == EINTR) continue; return -1; }
        if (w == 0) break;
        p += w; left -= (size_t)w;
    }
    return (ssize_t)(n - left);
}

/* All frames to the host are written from the single main thread. */
static void send_frame(uint32_t id, uint8_t type, const void *payload, uint32_t len) {
    unsigned char hdr[9];
    memcpy(hdr, &id, 4);
    hdr[4] = type;
    memcpy(hdr + 5, &len, 4);
    writen(1, hdr, 9);
    if (len) writen(1, payload, len);
}

static void send_exit(uint32_t id, uint32_t code) { send_frame(id, 6, &code, 4); }

static Sess *find_sess(uint32_t id) {
    for (int i = 0; i < MAX_SESS; i++)
        if (g_sess[i].active && g_sess[i].id == id) return &g_sess[i];
    return NULL;
}

static Sess *free_slot(void) {
    for (int i = 0; i < MAX_SESS; i++)
        if (!g_sess[i].active) return &g_sess[i];
    return NULL;
}

/* Close a session: optionally SIGHUP it, reap, and notify the host. */
static void end_sess(Sess *s, int killit) {
    if (!s || !s->active) return;
    uint32_t id = s->id;
    pid_t pid = s->pid;
    if (killit && pid > 0) kill(pid, SIGHUP);
    if (s->master >= 0) { close(s->master); s->master = -1; }
    int status = 0;
    if (pid > 0) waitpid(pid, &status, 0);
    s->active = 0; s->pid = -1;
    uint32_t code = WIFEXITED(status) ? (uint32_t)WEXITSTATUS(status)
                  : WIFSIGNALED(status) ? (uint32_t)(128 + WTERMSIG(status)) : 1u;
    send_exit(id, code);
}

static void open_sess(uint32_t id, uint16_t cols, uint16_t rows,
                      const char *cwd, const char *shell_req) {
    if (find_sess(id)) return;                         /* duplicate id */
    Sess *s = free_slot();
    if (!s) { send_exit(id, 127); return; }

    struct winsize ws;
    memset(&ws, 0, sizeof ws);
    ws.ws_col = cols ? cols : 80;
    ws.ws_row = rows ? rows : 24;

    int master;
    pid_t pid = forkpty(&master, NULL, NULL, &ws);
    if (pid < 0) { send_exit(id, 127); return; }

    if (pid == 0) {
        if (cwd && *cwd) { if (chdir(cwd) != 0) { /* ignore */ } }
        const char *shell = (shell_req && *shell_req) ? shell_req : getenv("SHELL");
        if (!shell || !*shell) shell = "/bin/bash";
        const char *base = strrchr(shell, '/');
        base = base ? base + 1 : shell;
        char arg0[256];
        snprintf(arg0, sizeof arg0, "-%s", base);      /* login shell */
        execl(shell, arg0, (char *)NULL);
        execlp("bash", "-bash", (char *)NULL);
        _exit(127);
    }

    s->id = id; s->master = master; s->pid = pid; s->active = 1;
}

/* ---- host -> server frame parsing ------------------------------------- */

static unsigned char *g_buf;
static size_t g_len, g_cap;

static uint32_t rd_u32(const unsigned char *p) { uint32_t v; memcpy(&v, p, 4); return v; }
static uint16_t rd_u16(const unsigned char *p) { uint16_t v; memcpy(&v, p, 2); return v; }

static void handle_open(uint32_t id, const unsigned char *p, uint32_t len) {
    if (len < 8) { send_exit(id, 127); return; }
    uint16_t cols = rd_u16(p), rows = rd_u16(p + 2);
    uint32_t off = 4;
    uint32_t cwd_len = rd_u32(p + off); off += 4;
    if (off + cwd_len > len) return;
    char *cwd = NULL;
    if (cwd_len) { cwd = malloc(cwd_len + 1); memcpy(cwd, p + off, cwd_len); cwd[cwd_len] = 0; }
    off += cwd_len;
    char *shell = NULL;
    if (off + 4 <= len) {
        uint32_t sh_len = rd_u32(p + off); off += 4;
        if (sh_len && off + sh_len <= len) { shell = malloc(sh_len + 1); memcpy(shell, p + off, sh_len); shell[sh_len] = 0; }
    }
    open_sess(id, cols, rows, cwd ? cwd : "", shell ? shell : "");
    free(cwd); free(shell);
}

static void feed_stdin(const unsigned char *data, size_t n) {
    if (g_len + n > g_cap) {
        size_t ncap = g_cap ? g_cap : 4096;
        while (ncap < g_len + n) ncap *= 2;
        g_buf = realloc(g_buf, ncap);
        g_cap = ncap;
    }
    memcpy(g_buf + g_len, data, n);
    g_len += n;

    size_t off = 0;
    while (g_len - off >= 9) {
        uint32_t id = rd_u32(g_buf + off);
        uint8_t type = g_buf[off + 4];
        uint32_t len = rd_u32(g_buf + off + 5);
        if (g_len - off < (size_t)9 + len) break;
        const unsigned char *p = g_buf + off + 9;
        Sess *s;
        switch (type) {
            case 1: handle_open(id, p, len); break;
            case 2: if ((s = find_sess(id))) writen(s->master, p, len); break;
            case 3:
                if ((s = find_sess(id)) && len >= 4) {
                    struct winsize ws; memset(&ws, 0, sizeof ws);
                    ws.ws_col = rd_u16(p); ws.ws_row = rd_u16(p + 2);
                    ioctl(s->master, TIOCSWINSZ, &ws);
                }
                break;
            case 4: if ((s = find_sess(id)) && len >= 1 && s->pid > 0) kill(s->pid, p[0]); break;
            case 5: end_sess(find_sess(id), 1); break;
            default: break;
        }
        off += 9 + len;
    }
    if (off) { memmove(g_buf, g_buf + off, g_len - off); g_len -= off; }
}

int main(void) {
    if (!getenv("TERM")) setenv("TERM", "xterm-256color", 1);
    signal(SIGPIPE, SIG_IGN);
    for (int i = 0; i < MAX_SESS; i++) { g_sess[i].active = 0; g_sess[i].master = -1; g_sess[i].pid = -1; }

    char buf[65536];
    struct pollfd fds[1 + MAX_SESS];
    Sess *pmap[1 + MAX_SESS];
    int infd = 0;

    for (;;) {
        int nf = 0;
        if (infd >= 0) { fds[nf].fd = infd; fds[nf].events = POLLIN; fds[nf].revents = 0; pmap[nf] = NULL; nf++; }
        for (int i = 0; i < MAX_SESS; i++) {
            if (!g_sess[i].active) continue;
            fds[nf].fd = g_sess[i].master; fds[nf].events = POLLIN; fds[nf].revents = 0; pmap[nf] = &g_sess[i]; nf++;
        }
        if (nf == 0) break;                            /* stdin closed and no sessions */

        int r = poll(fds, nf, -1);
        if (r < 0) { if (errno == EINTR) continue; break; }

        int host_gone = 0;
        for (int k = 0; k < nf; k++) {
            if (!fds[k].revents) continue;
            if (pmap[k] == NULL) {                     /* stdin (control channel) */
                if (fds[k].revents & POLLIN) {
                    ssize_t kk = read(infd, buf, sizeof buf);
                    if (kk > 0) feed_stdin((unsigned char *)buf, (size_t)kk);
                    else if (kk == 0) host_gone = 1;
                    else if (errno != EINTR) host_gone = 1;
                } else if (fds[k].revents & (POLLHUP | POLLERR)) {
                    host_gone = 1;
                }
            } else {                                   /* a pty master -> host */
                Sess *s = pmap[k];
                ssize_t kk = read(s->master, buf, sizeof buf);
                if (kk > 0) send_frame(s->id, 2, buf, (uint32_t)kk);
                else if (kk == 0 || errno != EINTR) end_sess(s, 0);   /* shell exited */
            }
        }

        if (host_gone) {                               /* Windows disconnected: tear down */
            for (int i = 0; i < MAX_SESS; i++) if (g_sess[i].active) end_sess(&g_sess[i], 1);
            break;
        }
    }

    free(g_buf);
    return 0;
}
