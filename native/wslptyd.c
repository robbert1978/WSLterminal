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
#include <sys/uio.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/socket.h>
#include <linux/vm_sockets.h>

#ifndef AF_VSOCK
#define AF_VSOCK 40
#endif

#define MAX_SESS 256

typedef struct {
    uint32_t id;
    int      master;
    pid_t    pid;
    int      active;
} Sess;

static Sess g_sess[MAX_SESS];

/* Transport fds for the active session loop: stdin/stdout (the wslg pipe path)
 * or the accepted vsock connection in --vsock mode. */
static int g_in = 0, g_out = 1;
/* The vsock listening socket (owner only), closed/cleaned up on shutdown. */
static volatile sig_atomic_t g_listen_fd = -1;
static const char *STAGE_PATH = "/tmp/wslptyd";
/* Live per-connection children; the listener auto-exits when it reaches 0,
 * unless --persist is set (e.g. under systemd, where Restart=always would just
 * cycle the service and the cgroup kill would tear down active sessions). */
static volatile sig_atomic_t g_active = 0;
static int g_persist = 0;
/* Zero-copy PTY->host forwarding: splice master -> pipe -> connection so the
 * payload never enters userspace. Disabled if the kernel rejects it. */
static int g_use_splice = 1;
static int g_pipe[2] = {-1, -1};
/* This distro's WSL registration name (self-detected via wslpath; see main).
 * Reported to the host once per connection so it never has to query wsl.exe. */
static char g_distro[128];

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

/* All frames to the host are written from the single main thread. The 9-byte
 * header and the payload go out in a single writev so each frame is one syscall
 * and (ideally) one vsock send instead of two — fewer syscalls, less
 * packetization on the latency-sensitive path. Loops past partial writes. */
static void send_frame(uint32_t id, uint8_t type, const void *payload, uint32_t len) {
    unsigned char hdr[9];
    memcpy(hdr, &id, 4);
    hdr[4] = type;
    memcpy(hdr + 5, &len, 4);

    struct iovec iov[2];
    iov[0].iov_base = hdr;
    iov[0].iov_len = 9;
    iov[1].iov_base = (void *)payload;
    iov[1].iov_len = len;
    int n = len ? 2 : 1, i = 0;
    while (i < n) {
        ssize_t w = writev(g_out, &iov[i], n - i);
        if (w < 0) { if (errno == EINTR) continue; return; }
        if (w == 0) break;
        size_t adv = (size_t)w;
        while (i < n && adv >= iov[i].iov_len) { adv -= iov[i].iov_len; i++; }
        if (i < n) {
            iov[i].iov_base = (char *)iov[i].iov_base + adv;
            iov[i].iov_len -= adv;
        }
    }
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
        /* Requested dir if it exists; else fall back to $HOME (~) — never the
         * daemon's cwd. (A translated Windows path may point at an unmounted
         * drive, so a failed chdir must still land in the home directory.) */
        if (!cwd || !*cwd || chdir(cwd) != 0) {
            const char *home = getenv("HOME");
            if (home && *home) { if (chdir(home) != 0) { /* ignore */ } }
        }
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

/* Move exactly n bytes from the pipe read end to the connection, looping past
 * partial moves / EINTR. Returns 0 on success, -1 if the connection broke. */
static int splice_drain(size_t n) {
    while (n) {
        ssize_t w = splice(g_pipe[0], NULL, g_out, NULL, n, SPLICE_F_MOVE);
        if (w < 0) { if (errno == EINTR) continue; return -1; }
        if (w == 0) return -1;
        n -= (size_t)w;
    }
    return 0;
}

/* Forward one chunk of a session's PTY output to the host. Zero-copy via splice
 * (master -> pipe -> connection): only the 9-byte DATA header passes through
 * userspace. Falls back to read()+send_frame() if splice is unsupported. */
static void forward_master(Sess *s, char *buf, size_t bufsz) {
    if (g_use_splice && g_pipe[0] >= 0) {
        ssize_t n = splice(s->master, NULL, g_pipe[1], NULL, 1 << 20, SPLICE_F_MOVE);
        if (n > 0) {
            unsigned char hdr[9];
            uint32_t len = (uint32_t)n;
            memcpy(hdr, &s->id, 4);
            hdr[4] = 2;                          /* DATA */
            memcpy(hdr + 5, &len, 4);
            writen(g_out, hdr, 9);
            splice_drain((size_t)n);             /* a write error => host_gone next poll */
            return;
        }
        if (n < 0 && (errno == EINVAL || errno == ENOSYS)) {
            g_use_splice = 0;                    /* kernel can't splice this -> copy path */
        } else {
            if (n == 0 || errno != EINTR) end_sess(s, 0);   /* shell exited */
            return;
        }
    }
    ssize_t kk = read(s->master, buf, bufsz);
    if (kk > 0) send_frame(s->id, 2, buf, (uint32_t)kk);
    else if (kk == 0 || errno != EINTR) end_sess(s, 0);
}

/* ---- per-connection session loop (g_in/g_out are the transport fds) ---- */

static void run_session_loop(void) {
    char buf[65536];
    /* Tell the host which distro this daemon serves (type 7, session 0), so it
     * can build \\wsl.localhost\<name> paths without querying wsl.exe itself. */
    if (g_distro[0]) send_frame(0, 7, g_distro, (uint32_t)strlen(g_distro));
    if (g_use_splice && g_pipe[0] < 0) {
        if (pipe2(g_pipe, O_CLOEXEC) == 0) {
            fcntl(g_pipe[0], F_SETPIPE_SZ, 1 << 18); /* larger chunks (best-effort) */
        } else {
            g_pipe[0] = g_pipe[1] = -1;
            g_use_splice = 0;
        }
    }
    struct pollfd fds[1 + MAX_SESS];
    Sess *pmap[1 + MAX_SESS];
    int infd = g_in;

    for (;;) {
        int nf = 0;
        if (infd >= 0) { fds[nf].fd = infd; fds[nf].events = POLLIN; fds[nf].revents = 0; pmap[nf] = NULL; nf++; }
        for (int i = 0; i < MAX_SESS; i++) {
            if (!g_sess[i].active) continue;
            fds[nf].fd = g_sess[i].master; fds[nf].events = POLLIN; fds[nf].revents = 0; pmap[nf] = &g_sess[i]; nf++;
        }
        if (nf == 0) break;                            /* host gone and no sessions */

        int r = poll(fds, nf, -1);
        if (r < 0) { if (errno == EINTR) continue; break; }

        int host_gone = 0;
        for (int k = 0; k < nf; k++) {
            if (!fds[k].revents) continue;
            if (pmap[k] == NULL) {                     /* host -> control channel */
                if (fds[k].revents & POLLIN) {
                    ssize_t kk = read(infd, buf, sizeof buf);
                    if (kk > 0) feed_stdin((unsigned char *)buf, (size_t)kk);
                    else if (kk == 0) host_gone = 1;
                    else if (errno != EINTR) host_gone = 1;
                } else if (fds[k].revents & (POLLHUP | POLLERR)) {
                    host_gone = 1;
                }
            } else {                                   /* a pty master -> host */
                forward_master(pmap[k], buf, sizeof buf);
            }
        }

        if (host_gone) {                               /* host disconnected: tear down */
            for (int i = 0; i < MAX_SESS; i++) if (g_sess[i].active) end_sess(&g_sess[i], 1);
            break;
        }
    }

    if (g_pipe[0] >= 0) { close(g_pipe[0]); close(g_pipe[1]); g_pipe[0] = g_pipe[1] = -1; }
    free(g_buf);
    g_buf = NULL; g_len = g_cap = 0;
}

/* ---- vsock server: one forked session loop per accepted client ---------- */

static void cleanup_and_exit(void) {
    if (g_listen_fd >= 0) close(g_listen_fd);
    unlink(STAGE_PATH);                                 /* owner cleans up the staged binary */
    _exit(0);
}

static void on_term(int sig) {
    (void)sig;
    cleanup_and_exit();
}

/* Reap connection children; when the last one is gone there are no windows left
 * to serve, so the listener exits (auto-shutdown — nothing lingers). */
static void on_chld(int sig) {
    (void)sig;
    while (waitpid(-1, NULL, WNOHANG) > 0) {
        if (g_active > 0) g_active--;
    }
    if (g_active == 0 && !g_persist) cleanup_and_exit();
}

static int vsock_serve(int port) {
    int ls = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (ls < 0) return 1;

    struct sockaddr_vm sa;
    memset(&sa, 0, sizeof sa);
    sa.svm_family = AF_VSOCK;
    sa.svm_cid = VMADDR_CID_ANY;
    sa.svm_port = (unsigned)port;
    if (bind(ls, (struct sockaddr *)&sa, sizeof sa) < 0) {
        int e = errno;
        close(ls);
        /* EADDRINUSE: another wslptyd already owns the port -> yield quietly and
         * do NOT remove the staged binary (the owner is running from it). */
        return (e == EADDRINUSE) ? 0 : 1;
    }
    if (listen(ls, 16) < 0) { close(ls); return 1; }

    g_listen_fd = ls;
    signal(SIGCHLD, on_chld);                           /* reap + auto-exit when idle */
    signal(SIGTERM, on_term);                           /* shutdown: close + rm staged bin */
    signal(SIGINT, on_term);

    for (;;) {
        int c = accept(ls, NULL, NULL);
        if (c < 0) { if (errno == EINTR) continue; break; }
        fcntl(c, F_SETFD, FD_CLOEXEC);                  /* don't leak the socket into shells */
        g_active++;                                     /* count before fork (SIGCHLD race) */
        pid_t pid = fork();
        if (pid == 0) {                                 /* connection child: own session loop */
            close(ls);
            g_listen_fd = -1;
            signal(SIGCHLD, SIG_DFL);                   /* so end_sess()'s waitpid works */
            signal(SIGTERM, SIG_DFL);
            signal(SIGINT, SIG_DFL);
            g_in = c;
            g_out = c;
            run_session_loop();
            _exit(0);
        }
        if (pid < 0) g_active--;                        /* fork failed */
        close(c);                                       /* parent keeps only the listener */
    }

    close(ls);
    unlink(STAGE_PATH);
    return 0;
}

/* Self-detect the WSL distro registration name WITHOUT relying on
 * $WSL_DISTRO_NAME — which WSL only sets for sessions it spawns directly, not
 * for a daemon reparented to init (or launched by systemd). `/init` dispatches
 * on argv[0]; invoked as "wslpath -m /" it prints the share path for `/`, e.g.
 * "//wsl.localhost/Ubuntu/", whose component after the host is the registered
 * name. Writes it to `out` and returns 1 on success, 0 on any failure. */
static int detect_distro_name(char *out, size_t outsz) {
    int pfd[2];
    if (pipe(pfd) != 0) return 0;
    pid_t pid = fork();
    if (pid < 0) { close(pfd[0]); close(pfd[1]); return 0; }
    if (pid == 0) {
        dup2(pfd[1], 1);
        close(pfd[0]); close(pfd[1]);
        int dn = open("/dev/null", O_WRONLY);
        if (dn >= 0) { dup2(dn, 2); close(dn); }
        char *const a[] = { "wslpath", "-m", "/", NULL };
        execv("/init", a);                              /* argv[0] selects mode */
        _exit(127);
    }
    close(pfd[1]);
    char buf[256];
    ssize_t n, total = 0;
    while (total < (ssize_t)sizeof buf - 1 &&
           (n = read(pfd[0], buf + total, sizeof buf - 1 - total)) > 0)
        total += n;
    close(pfd[0]);
    int st; waitpid(pid, &st, 0);
    if (total <= 0) return 0;
    buf[total] = 0;
    for (char *p = buf; *p; p++) if (*p == '\\') *p = '/'; /* normalize seps */
    while (total > 0 && (buf[total-1] == '\n' || buf[total-1] == '\r' ||
                         buf[total-1] == '/'  || buf[total-1] == ' '))
        buf[--total] = 0;                                /* trim trailing junk */
    char *p = buf;
    while (*p == '/') p++;                               /* skip leading // */
    char *slash = strchr(p, '/');                        /* end of host comp */
    if (!slash) return 0;
    char *name = slash + 1;                              /* distro starts here */
    char *end = strchr(name, '/');
    if (end) *end = 0;
    if (!*name) return 0;
    snprintf(out, outsz, "%s", name);
    return 1;
}

int main(int argc, char **argv) {
    if (!getenv("TERM")) setenv("TERM", "xterm-256color", 1);
    /* Let shells/apps detect this terminal (e.g. the shell-integration script). */
    setenv("WSLTERM", "1", 1);
    setenv("TERM_PROGRAM", "WSLTerminal", 1);
    /* Detect this distro's registration name once (prefer an existing
     * WSL_DISTRO_NAME, else ask wslpath). We both export it — so our shells look
     * like a native WSL session; forked children inherit it — and stash it in
     * g_distro to report to the host on each connection. */
    const char *have = getenv("WSL_DISTRO_NAME");
    if (have && *have) {
        snprintf(g_distro, sizeof g_distro, "%s", have);
    } else if (detect_distro_name(g_distro, sizeof g_distro)) {
        setenv("WSL_DISTRO_NAME", g_distro, 1);
    }
    signal(SIGPIPE, SIG_IGN);
    for (int i = 0; i < MAX_SESS; i++) { g_sess[i].active = 0; g_sess[i].master = -1; g_sess[i].pid = -1; }

    int port = -1;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--vsock") && i + 1 < argc) port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--persist")) g_persist = 1; /* don't auto-exit (systemd) */
    }
    if (port > 0)
      return vsock_serve(port);

    setsid();

    /* Legacy: a single connection over stdin/stdout (the wslg pipe path). */
    g_in = 0;
    g_out = 1;
    run_session_loop();
    return 0;
}
