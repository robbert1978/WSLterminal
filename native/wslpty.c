/*
 * wslpty — a tiny PTY bridge that runs *inside* a WSL distribution.
 *
 * It allocates a genuine Linux pseudo-terminal (forkpty -> /dev/ptmx),
 * runs the user's login shell as the session leader on the slave side,
 * and relays bytes between the pty master and its own stdin/stdout.
 *
 * The Windows side launches this helper via wslapi.dll's WslLaunch(),
 * handing us three raw Windows pipe handles as fd 0/1/2.  That means the
 * shell gets a real TTY (full job control, line discipline, SIGWINCH,
 * ioctl resize) WITHOUT going through conhost/ConPTY or the cooked
 * stdio pipe you get from invoking the wsl.exe console program.
 *
 * Wire protocol
 * -------------
 *   stdout (fd 1): raw pty output, forwarded verbatim to the host.
 *   stdin  (fd 0): a framed control+data stream from the host:
 *       type 0x00 DATA   : u32le len, then <len> bytes  -> write to pty master
 *       type 0x01 RESIZE : u16le cols, u16le rows        -> TIOCSWINSZ
 *       type 0x02 SIGNAL : u8 signo                       -> kill(child, signo)
 *   (multi-byte integers are little-endian; both WSL/x86-64 and Windows
 *    are little-endian so this is a straight memcpy on each side.)
 *
 * Test/standalone aid: `wslpty --exec <cmd> [args...]` runs <cmd> on the
 * pty instead of the login shell, which lets the protocol be exercised
 * end-to-end (e.g. `wslpty --exec sh -c 'tty'`).
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

static pid_t g_child  = -1;
static int   g_master = -1;

#define DBG(...) do { if (getenv("WSLPTY_DEBUG")) { \
        fprintf(stderr, "[wslpty] " __VA_ARGS__); fflush(stderr); } } while (0)

/* write all n bytes, retrying short writes and EINTR */
static ssize_t writen(int fd, const void *buf, size_t n) {
    const char *p = (const char *)buf;
    size_t left = n;
    while (left) {
        ssize_t w = write(fd, p, left);
        if (w < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (w == 0) break;
        p += w;
        left -= (size_t)w;
    }
    return (ssize_t)(n - left);
}

/*
 * Append a freshly-read chunk to the frame buffer and consume every
 * complete frame currently available.  Leftover partial-frame bytes are
 * kept at the front of the buffer for the next call.  Returns -1 on a
 * fatal error (write to pty failed or OOM), 0 otherwise.
 */
static int feed_frames(unsigned char **pbuf, size_t *plen, size_t *pcap,
                       const unsigned char *data, size_t n) {
    if (*plen + n > *pcap) {
        size_t ncap = *pcap ? *pcap : 256;
        while (ncap < *plen + n) ncap *= 2;
        unsigned char *nb = (unsigned char *)realloc(*pbuf, ncap);
        if (!nb) return -1;
        *pbuf = nb;
        *pcap = ncap;
    }
    memcpy(*pbuf + *plen, data, n);
    *plen += n;

    size_t off = 0;
    unsigned char *b = *pbuf;
    for (;;) {
        size_t avail = *plen - off;
        if (avail < 1) break;
        unsigned char type = b[off];

        if (type == 0x00) {                 /* DATA */
            if (avail < 5) break;
            uint32_t len;
            memcpy(&len, b + off + 1, 4);
            if (avail < (size_t)5 + len) break;
            if (g_master >= 0 && len &&
                writen(g_master, b + off + 5, len) < 0)
                return -1;
            off += 5 + len;
        } else if (type == 0x01) {          /* RESIZE */
            if (avail < 5) break;
            uint16_t cols, rows;
            memcpy(&cols, b + off + 1, 2);
            memcpy(&rows, b + off + 3, 2);
            struct winsize ws;
            memset(&ws, 0, sizeof ws);
            ws.ws_col = cols;
            ws.ws_row = rows;
            if (g_master >= 0) ioctl(g_master, TIOCSWINSZ, &ws);
            off += 5;
        } else if (type == 0x02) {          /* SIGNAL */
            if (avail < 2) break;
            int sig = b[off + 1];
            if (g_child > 0) kill(g_child, sig);
            off += 2;
        } else {
            /* unknown framing byte: drop it and resync */
            off += 1;
        }
    }

    if (off) {
        memmove(b, b + off, *plen - off);
        *plen -= off;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (!getenv("TERM")) setenv("TERM", "xterm-256color", 1);

    struct winsize ws;
    memset(&ws, 0, sizeof ws);
    ws.ws_col = 80;
    ws.ws_row = 24;

    pid_t pid = forkpty(&g_master, NULL, NULL, &ws);
    if (pid < 0) {
        perror("wslpty: forkpty");
        return 127;
    }

    if (pid == 0) {
        /* ---- child: now the session leader with the pty as its tty ---- */
        if (argc > 2 && strcmp(argv[1], "--exec") == 0) {
            execvp(argv[2], &argv[2]);
            perror("wslpty: execvp");
            _exit(127);
        }
        const char *shell = getenv("SHELL");
        if (!shell || !*shell) shell = "/bin/bash";
        const char *base = strrchr(shell, '/');
        base = base ? base + 1 : shell;
        char arg0[256];
        snprintf(arg0, sizeof arg0, "-%s", base);   /* leading '-' => login shell */
        execl(shell, arg0, (char *)NULL);
        execlp("bash", "-bash", (char *)NULL);       /* last-ditch fallback */
        perror("wslpty: exec shell");
        _exit(127);
    }

    /* ---------------------------- parent ---------------------------- */
    g_child = pid;
    signal(SIGPIPE, SIG_IGN);
    DBG("forkpty ok: child=%d master=%d\n", (int)pid, g_master);

    unsigned char *fbuf = NULL;
    size_t flen = 0, fcap = 0;
    char buf[65536];
    int infd = 0;             /* host control/data channel; -1 once it closes */

    for (;;) {
        struct pollfd fds[2];
        fds[0].fd = infd;     /* poll ignores fds with fd < 0 */
        fds[0].events = POLLIN; fds[0].revents = 0;
        fds[1].fd = g_master; fds[1].events = POLLIN; fds[1].revents = 0;

        int r = poll(fds, 2, -1);
        if (r < 0) {
            if (errno == EINTR) continue;
            break;
        }
        DBG("poll r=%d in.rev=0x%x master.rev=0x%x\n",
            r, fds[0].revents, fds[1].revents);

        /* pty master -> host stdout */
        if (fds[1].revents & (POLLIN | POLLHUP | POLLERR)) {
            ssize_t k = read(g_master, buf, sizeof buf);
            DBG("master read k=%zd errno=%d\n", k, k < 0 ? errno : 0);
            if (k > 0) {
                if (writen(1, buf, (size_t)k) < 0) break;
            } else if (k == 0) {
                break;                       /* slave closed: shell gone */
            } else if (errno != EINTR) {
                break;                       /* EIO after child exit */
            }
        }

        /* host stdin (framed) -> pty master.  EOF/hangup here means the
         * host stopped sending input; it must NOT tear down the shell —
         * the shell may still be producing output.  We just stop watching
         * stdin and let the master/shell lifecycle decide when we're done. */
        if (fds[0].revents & POLLIN) {
            ssize_t k = read(infd, buf, sizeof buf);
            if (k > 0) {
                if (feed_frames(&fbuf, &flen, &fcap,
                                (unsigned char *)buf, (size_t)k) < 0)
                    break;
            } else if (k == 0) {
                DBG("stdin EOF; stop watching control channel\n");
                infd = -1;
            } else if (errno != EINTR) {
                infd = -1;
            }
        } else if (fds[0].revents & (POLLHUP | POLLERR)) {
            DBG("stdin HUP/ERR; stop watching control channel\n");
            infd = -1;
        }
    }

    if (g_child > 0) kill(g_child, SIGHUP);

    int status = 0;
    if (g_child > 0) waitpid(g_child, &status, 0);

    /* best-effort drain of any final pty output */
    for (;;) {
        ssize_t k = read(g_master, buf, sizeof buf);
        if (k > 0) writen(1, buf, (size_t)k);
        else break;
    }

    free(fbuf);
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    return 1;
}
