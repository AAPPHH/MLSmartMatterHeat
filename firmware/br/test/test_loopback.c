#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
typedef int socklen_t_;
#else
#include <arpa/inet.h>
#include <sys/socket.h>
#include <unistd.h>
#include <poll.h>
#endif
#include "../main/matter.h"
#include "vectors.h"

typedef struct {
    int fd;
    struct sockaddr_in peer;
    int sent;
} net_t;

static int net_send(void *ctx, const uint8_t *b, size_t n)
{
    net_t *s = ctx;
    s->sent++;
    return (int)sendto(s->fd, (const char *)b, (int)n, 0, (struct sockaddr *)&s->peer, sizeof(s->peer));
}

static int net_recv(void *ctx, uint8_t *b, size_t max, uint32_t ms)
{
    net_t *s = ctx;
    fd_set fds;
    struct timeval tv = {(long)(ms / 1000), (long)(ms % 1000) * 1000};
    FD_ZERO(&fds);
    FD_SET((unsigned)s->fd, &fds);
    int r = select(s->fd + 1, &fds, NULL, NULL, &tv);
    if (r <= 0) return 0;
    return (int)recv(s->fd, (char *)b, (int)max, 0);
}

static uint32_t net_now(void *ctx)
{
    (void)ctx;
    struct timespec t;
    timespec_get(&t, TIME_UTC);
    return (uint32_t)(t.tv_sec * 1000 + t.tv_nsec / 1000000);
}

static void net_rng(void *ctx, uint8_t *b, size_t n)
{
    (void)ctx;
    for (size_t i = 0; i < n; i++) b[i] = (uint8_t)(rand() >> 3);
}

int main(int argc, char **argv)
{
    if (argc < 4) return 2;
    int port = atoi(argv[1]);
    const char *expect = argv[2];
    uint64_t peer = strtoull(argv[3], NULL, 16);
#ifdef _WIN32
    WSADATA wsa;
    WSAStartup(MAKEWORD(2, 2), &wsa);
#endif
    srand((unsigned)time(NULL));
    net_t n;
    memset(&n, 0, sizeof(n));
    n.fd = (int)socket(AF_INET, SOCK_DGRAM, 0);
    n.peer.sin_family = AF_INET;
    n.peer.sin_port = htons((uint16_t)port);
    n.peer.sin_addr.s_addr = inet_addr("127.0.0.1");
    mc_io_t io = {&n, net_send, net_recv, net_now, net_rng};
    static wd_creds_t cr;
    mc_cert_t c;
    mc_cert_parse(V_Node01_01_Chip, sizeof(V_Node01_01_Chip), &c);
    cr.fabric_id = c.fabric_id;
    cr.node_id = c.node_id;
    memcpy(cr.rcac, V_Root01_Chip, sizeof(V_Root01_Chip));
    cr.rcac_len = sizeof(V_Root01_Chip);
    memcpy(cr.icac, V_ICA01_Chip, sizeof(V_ICA01_Chip));
    cr.icac_len = sizeof(V_ICA01_Chip);
    memcpy(cr.noc, V_Node01_01_Chip, sizeof(V_Node01_01_Chip));
    cr.noc_len = sizeof(V_Node01_01_Chip);
    memcpy(cr.key, c.pub, MC_PUB_LEN);
    memcpy(cr.key + MC_PUB_LEN, V_Node01_01_PrivateKey, 32);
    static const uint8_t ipk[16] = {0x23, 0x5b, 0xf7, 0xe6, 0x28, 0x23, 0xd3, 0x58, 0xdc, 0xa4, 0xba, 0x50, 0xb1, 0x53, 0x5f, 0x4b};
    memcpy(cr.ipk, ipk, 16);
    if (getenv("LB_BAD_KEY")) cr.key[70] ^= 1;
    if (getenv("LB_BAD_IPK")) cr.ipk[0] ^= 1;
    mc_mrp_t mrp = {300, 200, 2000, 5, 3000};
    if (getenv("LB_SLOW")) mrp.idle_ms = 100;
    char err[WD_ERR_LEN];
    uint32_t t0 = net_now(NULL);
    int rc = mc_write_int16(&io, &cr, peer, &mrp, 1, 0x0201, 0x0012, 1800, err, sizeof(err));
    printf("loopback rc=%d err=%s sent=%d ms=%u\n", rc, err, n.sent, (unsigned)(net_now(NULL) - t0));
    if (!strcmp(expect, "ok")) return rc == 0 ? 0 : 1;
    return rc != 0 && strstr(err, expect) ? 0 : 1;
}
