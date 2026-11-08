/* netprobe: prints what the Windows networking APIs report inside a Wine
 * prefix, then exercises each adapter address against the Internet. Built
 * by build.sh with the payload's mingw compiler; run under the engine with
 * `applejack prefix exec <prefix> -- Z:\...\netprobe.exe`. Output is plain
 * text for a report; every WSA error is printed as its number. */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <iphlpapi.h>
#include <windns.h>
#include <windows.h>
#include <winhttp.h>
#include <stdio.h>
#include <string.h>

#define DNS_HOST "api.steampowered.com"
#define CM_HOST "cm.steampowered.com"
#define PROBE_TIMEOUT_MS 3000

static const char *if_type_name(DWORD type)
{
    switch (type)
    {
    case IF_TYPE_OTHER: return "OTHER";
    case IF_TYPE_ETHERNET_CSMACD: return "ETHERNET";
    case IF_TYPE_PPP: return "PPP";
    case IF_TYPE_SOFTWARE_LOOPBACK: return "LOOPBACK";
    case IF_TYPE_IEEE80211: return "IEEE80211";
    case IF_TYPE_TUNNEL: return "TUNNEL";
    case IF_TYPE_SLIP: return "SLIP";
    default: return "?";
    }
}

static const char *oper_status_name(IF_OPER_STATUS status)
{
    switch (status)
    {
    case IfOperStatusUp: return "Up";
    case IfOperStatusDown: return "Down";
    case IfOperStatusTesting: return "Testing";
    case IfOperStatusUnknown: return "Unknown";
    case IfOperStatusDormant: return "Dormant";
    case IfOperStatusNotPresent: return "NotPresent";
    case IfOperStatusLowerLayerDown: return "LowerLayerDown";
    default: return "?";
    }
}

static const char *conn_type_name(NET_IF_CONNECTION_TYPE type)
{
    switch (type)
    {
    case NET_IF_CONNECTION_DEDICATED: return "Dedicated";
    case NET_IF_CONNECTION_PASSIVE: return "Passive";
    case NET_IF_CONNECTION_DEMAND: return "Demand";
    default: return "?";
    }
}

static const char *addr_str(const struct sockaddr *sa, char *buf, size_t len)
{
    DWORD size = (DWORD)len;
    if (!sa) return "(null)";
    if (WSAAddressToStringA((struct sockaddr *)sa, sa->sa_family == AF_INET6 ? sizeof(struct sockaddr_in6) : sizeof(struct sockaddr_in), NULL, buf, &size))
        snprintf(buf, len, "(WSAAddressToString error %d)", WSAGetLastError());
    return buf;
}

static void print_adapters_addresses(void)
{
    ULONG flags = GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS;
    ULONG size = 0;
    IP_ADAPTER_ADDRESSES *list, *aa;
    DWORD err;
    char buf[128];

    printf("== GetAdaptersAddresses(AF_UNSPEC, PREFIX|GATEWAYS)\n");
    err = GetAdaptersAddresses(AF_UNSPEC, flags, NULL, NULL, &size);
    if (err != ERROR_BUFFER_OVERFLOW) { printf("  error %lu sizing\n", err); return; }
    list = malloc(size);
    if (!list) return;
    err = GetAdaptersAddresses(AF_UNSPEC, flags, NULL, list, &size);
    if (err) { printf("  error %lu\n", err); free(list); return; }
    for (aa = list; aa; aa = aa->Next)
    {
        IP_ADAPTER_UNICAST_ADDRESS *ua;
        IP_ADAPTER_GATEWAY_ADDRESS *ga;
        IP_ADAPTER_DNS_SERVER_ADDRESS *da;
        printf("  [%lu] name=%s friendly=\"%ls\" desc=\"%ls\"\n", aa->IfIndex, aa->AdapterName, aa->FriendlyName, aa->Description);
        printf("       IfType=%lu(%s) OperStatus=%d(%s) Ipv4Metric=%lu Ipv6Metric=%lu ConnectionType=%d(%s) Flags=0x%lx Ipv6IfIndex=%lu Mtu=%lu\n",
               aa->IfType, if_type_name(aa->IfType), aa->OperStatus, oper_status_name(aa->OperStatus),
               aa->Ipv4Metric, aa->Ipv6Metric, aa->ConnectionType, conn_type_name(aa->ConnectionType),
               aa->Flags, aa->Ipv6IfIndex, aa->Mtu);
        for (ua = aa->FirstUnicastAddress; ua; ua = ua->Next)
            printf("       unicast %s/%u\n", addr_str(ua->Address.lpSockaddr, buf, sizeof(buf)), ua->OnLinkPrefixLength);
        for (ga = aa->FirstGatewayAddress; ga; ga = ga->Next)
            printf("       gateway %s\n", addr_str(ga->Address.lpSockaddr, buf, sizeof(buf)));
        for (da = aa->FirstDnsServerAddress; da; da = da->Next)
            printf("       dns %s\n", addr_str(da->Address.lpSockaddr, buf, sizeof(buf)));
        if (aa->DnsSuffix && aa->DnsSuffix[0]) printf("       dnssuffix \"%ls\"\n", aa->DnsSuffix);
    }
    free(list);
}

static void print_adapters_info(void)
{
    ULONG size = 0;
    IP_ADAPTER_INFO *list, *ai;
    DWORD err;

    printf("== GetAdaptersInfo order\n");
    err = GetAdaptersInfo(NULL, &size);
    if (err != ERROR_BUFFER_OVERFLOW) { printf("  error %lu sizing\n", err); return; }
    list = malloc(size);
    if (!list) return;
    err = GetAdaptersInfo(list, &size);
    if (err) { printf("  error %lu\n", err); free(list); return; }
    for (ai = list; ai; ai = ai->Next)
    {
        IP_ADDR_STRING *s;
        printf("  [%lu] %s type=%u desc=\"%s\" dhcp=%u\n", ai->Index, ai->AdapterName, ai->Type, ai->Description, ai->DhcpEnabled);
        for (s = &ai->IpAddressList; s; s = s->Next)
            if (s->IpAddress.String[0]) printf("       ip %s mask %s\n", s->IpAddress.String, s->IpMask.String);
        for (s = &ai->GatewayList; s; s = s->Next)
            if (s->IpAddress.String[0]) printf("       gateway %s\n", s->IpAddress.String);
    }
    free(list);
}

static void print_network_params(void)
{
    ULONG size = 0;
    FIXED_INFO *info;
    IP_ADDR_STRING *s;
    DWORD err;

    printf("== GetNetworkParams\n");
    err = GetNetworkParams(NULL, &size);
    if (err != ERROR_BUFFER_OVERFLOW) { printf("  error %lu sizing\n", err); return; }
    info = malloc(size);
    if (!info) return;
    err = GetNetworkParams(info, &size);
    if (err) { printf("  error %lu\n", err); free(info); return; }
    printf("  host=%s domain=\"%s\" nodetype=%u\n", info->HostName, info->DomainName, info->NodeType);
    for (s = &info->DnsServerList; s; s = s->Next)
        if (s->IpAddress.String[0]) printf("  dns %s\n", s->IpAddress.String);
    free(info);
}

static void print_forward_table(void)
{
    ULONG size = 0;
    MIB_IPFORWARDTABLE *table;
    DWORD err, i;

    printf("== GetIpForwardTable(sort=0)\n");
    err = GetIpForwardTable(NULL, &size, FALSE);
    if (err != ERROR_INSUFFICIENT_BUFFER) { printf("  error %lu sizing\n", err); return; }
    table = malloc(size);
    if (!table) return;
    err = GetIpForwardTable(table, &size, FALSE);
    if (err) { printf("  error %lu\n", err); free(table); return; }
    for (i = 0; i < table->dwNumEntries; i++)
    {
        MIB_IPFORWARDROW *r = &table->table[i];
        struct in_addr dest, mask, hop;
        char d[32], m[32], h[32];
        dest.s_addr = r->dwForwardDest;
        mask.s_addr = r->dwForwardMask;
        hop.s_addr = r->dwForwardNextHop;
        strcpy(d, inet_ntoa(dest));
        strcpy(m, inet_ntoa(mask));
        strcpy(h, inet_ntoa(hop));
        printf("  dest=%s mask=%s nexthop=%s ifindex=%lu type=%lu proto=%lu metric1=%lu\n",
               d, m, h, r->dwForwardIfIndex, r->dwForwardType, r->dwForwardProto, r->dwForwardMetric1);
    }
    free(table);
}

static int resolve_first(const char *host, int family, struct sockaddr_storage *out, int *out_len)
{
    struct addrinfo hints, *res, *p;
    int err;

    memset(&hints, 0, sizeof(hints));
    hints.ai_family = family;
    hints.ai_socktype = SOCK_STREAM;
    err = getaddrinfo(host, NULL, &hints, &res);
    if (err) return err;
    for (p = res; p; p = p->ai_next)
    {
        if (p->ai_family == family)
        {
            memcpy(out, p->ai_addr, p->ai_addrlen);
            *out_len = (int)p->ai_addrlen;
            freeaddrinfo(res);
            return 0;
        }
    }
    freeaddrinfo(res);
    return WSAHOST_NOT_FOUND;
}

static void print_getaddrinfo(const char *host)
{
    struct addrinfo hints, *res, *p;
    int err;
    char buf[128];

    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    printf("== getaddrinfo(%s)\n", host);
    err = getaddrinfo(host, NULL, &hints, &res);
    if (err) { printf("  error %d\n", err); return; }
    for (p = res; p; p = p->ai_next)
        printf("  %s\n", addr_str(p->ai_addr, buf, sizeof(buf)));
    freeaddrinfo(res);
}

static void print_best_interface(void)
{
    DWORD index = 0, err;
    struct sockaddr_in dst;
    struct sockaddr_storage api;
    int api_len = 0;
    MIB_IPFORWARDROW row;
    char buf[128];

    printf("== GetBestInterfaceEx / GetBestRoute\n");
    memset(&dst, 0, sizeof(dst));
    dst.sin_family = AF_INET;
    dst.sin_addr.s_addr = inet_addr("8.8.8.8");
    err = GetBestInterfaceEx((struct sockaddr *)&dst, &index);
    printf("  GetBestInterfaceEx(8.8.8.8) -> err=%lu index=%lu\n", err, index);
    memset(&row, 0, sizeof(row));
    err = GetBestRoute(dst.sin_addr.s_addr, 0, &row);
    {
        struct in_addr a, m, h;
        a.s_addr = row.dwForwardDest; m.s_addr = row.dwForwardMask; h.s_addr = row.dwForwardNextHop;
        printf("  GetBestRoute(8.8.8.8) -> err=%lu ifindex=%lu dest=%s", err, row.dwForwardIfIndex, inet_ntoa(a));
        printf(" mask=%s", inet_ntoa(m));
        printf(" nexthop=%s metric1=%lu\n", inet_ntoa(h), row.dwForwardMetric1);
    }
    if (!resolve_first(DNS_HOST, AF_INET, &api, &api_len))
    {
        index = 0;
        err = GetBestInterfaceEx((struct sockaddr *)&api, &index);
        printf("  GetBestInterfaceEx(%s=%s) -> err=%lu index=%lu\n", DNS_HOST, addr_str((struct sockaddr *)&api, buf, sizeof(buf)), err, index);
    }
    else printf("  %s did not resolve to IPv4\n", DNS_HOST);
}

/* A minimal DNS A query for DNS_HOST; returns the length written. */
static int build_dns_query(unsigned char *buf, size_t len, unsigned short id)
{
    const char *host = DNS_HOST;
    unsigned char *p = buf;
    const char *label = host;

    if (len < 12 + strlen(host) + 2 + 4) return -1;
    p[0] = (unsigned char)(id >> 8); p[1] = (unsigned char)id;
    p[2] = 0x01; p[3] = 0x00; /* RD */
    p[4] = 0; p[5] = 1;       /* QDCOUNT */
    p[6] = p[7] = p[8] = p[9] = p[10] = p[11] = 0;
    p += 12;
    while (*label)
    {
        const char *dot = strchr(label, '.');
        size_t n = dot ? (size_t)(dot - label) : strlen(label);
        *p++ = (unsigned char)n;
        memcpy(p, label, n);
        p += n;
        label += n;
        if (*label == '.') label++;
    }
    *p++ = 0;
    *p++ = 0; *p++ = 1; /* QTYPE A */
    *p++ = 0; *p++ = 1; /* QCLASS IN */
    return (int)(p - buf);
}

static void describe_dns_reply(const unsigned char *r, int n)
{
    int qd, an, i, pos;

    if (n < 12) { printf("short reply (%d bytes)", n); return; }
    qd = (r[4] << 8) | r[5];
    an = (r[6] << 8) | r[7];
    printf("rcode=%d qd=%d an=%d", r[3] & 0x0f, qd, an);
    pos = 12;
    for (i = 0; i < qd && pos < n; i++)
    {
        while (pos < n && r[pos]) pos += r[pos] + 1;
        pos += 5;
    }
    for (i = 0; i < an && pos + 12 <= n; i++)
    {
        int type, rdlen;
        if (r[pos] & 0xc0) pos += 2;
        else { while (pos < n && r[pos]) pos += r[pos] + 1; pos++; }
        type = (r[pos] << 8) | r[pos + 1];
        rdlen = (r[pos + 8] << 8) | r[pos + 9];
        pos += 10;
        if (type == 1 && rdlen == 4 && pos + 4 <= n)
        {
            printf(" A=%u.%u.%u.%u", r[pos], r[pos + 1], r[pos + 2], r[pos + 3]);
            break;
        }
        pos += rdlen;
    }
}

/* Sends one A query to server from a socket bound to local (or unbound) and
 * waits PROBE_TIMEOUT_MS for the reply. */
static void dns_probe(const struct sockaddr *server, int server_len, const struct sockaddr *local, int local_len)
{
    SOCKET s;
    unsigned char q[512], r[512];
    int n, rc;
    fd_set rd;
    struct timeval tv = { PROBE_TIMEOUT_MS / 1000, (PROBE_TIMEOUT_MS % 1000) * 1000 };
    struct sockaddr_in6 to;
    char buf[128];

    memcpy(&to, server, server_len);
    if (to.sin6_family == AF_INET) ((struct sockaddr_in *)&to)->sin_port = htons(53);
    else to.sin6_port = htons(53);

    s = socket(server->sa_family, SOCK_DGRAM, IPPROTO_UDP);
    if (s == INVALID_SOCKET) { printf("socket error %d", WSAGetLastError()); return; }
    if (local && bind(s, local, local_len)) { printf("bind error %d", WSAGetLastError()); closesocket(s); return; }
    n = build_dns_query(q, sizeof(q), 0x4a4a);
    rc = sendto(s, (char *)q, n, 0, (struct sockaddr *)&to, server_len);
    if (rc == SOCKET_ERROR) { printf("sendto %s error %d", addr_str((struct sockaddr *)&to, buf, sizeof(buf)), WSAGetLastError()); closesocket(s); return; }
    FD_ZERO(&rd);
    FD_SET(s, &rd);
    rc = select(0, &rd, NULL, NULL, &tv);
    if (rc == 0) { printf("sendto ok, no reply in %d ms", PROBE_TIMEOUT_MS); closesocket(s); return; }
    if (rc == SOCKET_ERROR) { printf("select error %d", WSAGetLastError()); closesocket(s); return; }
    n = recvfrom(s, (char *)r, sizeof(r), 0, NULL, NULL);
    if (n == SOCKET_ERROR) { printf("recvfrom error %d", WSAGetLastError()); closesocket(s); return; }
    printf("reply %d bytes: ", n);
    describe_dns_reply(r, n);
    closesocket(s);
}

static void print_dns_probes(void)
{
    ULONG flags = GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS;
    ULONG size = 0;
    IP_ADAPTER_ADDRESSES *list, *aa;
    struct sockaddr_in google;
    char buf[128];

    printf("== UDP DNS A query for %s to each reported server\n", DNS_HOST);
    if (GetAdaptersAddresses(AF_UNSPEC, flags, NULL, NULL, &size) == ERROR_BUFFER_OVERFLOW && (list = malloc(size)))
    {
        if (!GetAdaptersAddresses(AF_UNSPEC, flags, NULL, list, &size))
        {
            for (aa = list; aa; aa = aa->Next)
            {
                IP_ADAPTER_DNS_SERVER_ADDRESS *da;
                for (da = aa->FirstDnsServerAddress; da; da = da->Next)
                {
                    printf("  [%lu] %s: ", aa->IfIndex, addr_str(da->Address.lpSockaddr, buf, sizeof(buf)));
                    dns_probe(da->Address.lpSockaddr, da->Address.iSockaddrLength, NULL, 0);
                    printf("\n");
                }
            }
        }
        free(list);
    }
    memset(&google, 0, sizeof(google));
    google.sin_family = AF_INET;
    google.sin_addr.s_addr = inet_addr("8.8.8.8");
    printf("  8.8.8.8: ");
    dns_probe((struct sockaddr *)&google, sizeof(google), NULL, 0);
    printf("\n");
}

static void tcp_probe(const struct sockaddr *local, int local_len, const struct sockaddr *remote, int remote_len)
{
    SOCKET s;
    u_long nb = 1;
    int rc, err = 0, err_len = sizeof(err);
    fd_set wr, ex;
    struct timeval tv = { PROBE_TIMEOUT_MS / 1000, (PROBE_TIMEOUT_MS % 1000) * 1000 };
    struct sockaddr_storage name;
    int name_len = sizeof(name);
    char buf[128];

    s = socket(remote->sa_family, SOCK_STREAM, IPPROTO_TCP);
    if (s == INVALID_SOCKET) { printf("socket error %d", WSAGetLastError()); return; }
    if (bind(s, local, local_len)) { printf("bind error %d", WSAGetLastError()); closesocket(s); return; }
    ioctlsocket(s, FIONBIO, &nb);
    rc = connect(s, remote, remote_len);
    if (rc == SOCKET_ERROR && WSAGetLastError() != WSAEWOULDBLOCK)
    {
        printf("connect error %d (immediate)", WSAGetLastError());
        closesocket(s);
        return;
    }
    FD_ZERO(&wr); FD_ZERO(&ex);
    FD_SET(s, &wr); FD_SET(s, &ex);
    rc = select(0, NULL, &wr, &ex, &tv);
    if (rc == 0) { printf("connect timeout after %d ms", PROBE_TIMEOUT_MS); closesocket(s); return; }
    if (rc == SOCKET_ERROR) { printf("select error %d", WSAGetLastError()); closesocket(s); return; }
    getsockopt(s, SOL_SOCKET, SO_ERROR, (char *)&err, &err_len);
    if (FD_ISSET(s, &ex) || err) printf("connect error %d (deferred)", err);
    else
    {
        getsockname(s, (struct sockaddr *)&name, &name_len);
        printf("connected, local %s", addr_str((struct sockaddr *)&name, buf, sizeof(buf)));
    }
    closesocket(s);
}

static void print_per_address_probes(void)
{
    ULONG flags = GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS;
    ULONG size = 0;
    IP_ADAPTER_ADDRESSES *list, *aa;
    struct sockaddr_storage api4, api6;
    int api4_len = 0, api6_len = 0, have4, have6;
    struct sockaddr_in google4;
    struct sockaddr_in6 google6;
    char buf[128];

    printf("== per adapter address: bind, TCP connect %s:443, UDP DNS to Google\n", DNS_HOST);
    have4 = !resolve_first(DNS_HOST, AF_INET, &api4, &api4_len);
    have6 = !resolve_first(DNS_HOST, AF_INET6, &api6, &api6_len);
    if (have4) ((struct sockaddr_in *)&api4)->sin_port = htons(443);
    if (have6) ((struct sockaddr_in6 *)&api6)->sin6_port = htons(443);
    memset(&google4, 0, sizeof(google4));
    google4.sin_family = AF_INET;
    google4.sin_addr.s_addr = inet_addr("8.8.8.8");
    memset(&google6, 0, sizeof(google6));
    google6.sin6_family = AF_INET6;
    inet_pton(AF_INET6, "2001:4860:4860::8888", &google6.sin6_addr);

    if (GetAdaptersAddresses(AF_UNSPEC, flags, NULL, NULL, &size) != ERROR_BUFFER_OVERFLOW || !(list = malloc(size))) return;
    if (GetAdaptersAddresses(AF_UNSPEC, flags, NULL, list, &size)) { free(list); return; }
    for (aa = list; aa; aa = aa->Next)
    {
        IP_ADAPTER_UNICAST_ADDRESS *ua;
        for (ua = aa->FirstUnicastAddress; ua; ua = ua->Next)
        {
            struct sockaddr_storage local;
            int local_len = ua->Address.iSockaddrLength;
            int v4 = ua->Address.lpSockaddr->sa_family == AF_INET;
            memcpy(&local, ua->Address.lpSockaddr, local_len);
            if (v4) ((struct sockaddr_in *)&local)->sin_port = 0;
            else ((struct sockaddr_in6 *)&local)->sin6_port = 0;
            printf("  [%lu] %s\n", aa->IfIndex, addr_str((struct sockaddr *)&local, buf, sizeof(buf)));
            printf("       tcp: ");
            if (v4 && have4) tcp_probe((struct sockaddr *)&local, local_len, (struct sockaddr *)&api4, api4_len);
            else if (!v4 && have6) tcp_probe((struct sockaddr *)&local, local_len, (struct sockaddr *)&api6, api6_len);
            else printf("no %s address for %s", v4 ? "IPv4" : "IPv6", DNS_HOST);
            printf("\n       udp: ");
            if (v4) dns_probe((struct sockaddr *)&google4, sizeof(google4), (struct sockaddr *)&local, local_len);
            else dns_probe((struct sockaddr *)&google6, sizeof(google6), (struct sockaddr *)&local, local_len);
            printf("\n");
        }
    }
    free(list);
}

/* The local-address discovery idiom: connect a UDP socket to a public
 * address and read the local address back with getsockname. Wine's
 * GetBestRoute2 does this with the caller's port, which is 0 when the Steam
 * client calls GetBestInterfaceEx (U22a); 0004 sets port 9 there. */
static void print_udp_connect_trick(void)
{
    static const unsigned short ports[] = { 0, 53 };
    size_t i;
    char buf[128];

    printf("== UDP connect(8.8.8.8:port) + getsockname\n");
    for (i = 0; i < sizeof(ports) / sizeof(ports[0]); i++)
    {
        SOCKET s = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
        struct sockaddr_in dst, name;
        int name_len = sizeof(name);
        memset(&dst, 0, sizeof(dst));
        dst.sin_family = AF_INET;
        dst.sin_port = htons(ports[i]);
        dst.sin_addr.s_addr = inet_addr("8.8.8.8");
        printf("  port %u: ", ports[i]);
        if (connect(s, (struct sockaddr *)&dst, sizeof(dst))) printf("connect error %d\n", WSAGetLastError());
        else if (getsockname(s, (struct sockaddr *)&name, &name_len)) printf("getsockname error %d\n", WSAGetLastError());
        else printf("local %s\n", addr_str((struct sockaddr *)&name, buf, sizeof(buf)));
        closesocket(s);
    }
}

static void print_dnsquery(void)
{
    DNS_RECORDA *rec = NULL, *r;
    DNS_STATUS status;

    printf("== DnsQuery_A(%s, A)\n", DNS_HOST);
    status = DnsQuery_A(DNS_HOST, DNS_TYPE_A, DNS_QUERY_STANDARD, NULL, &rec, NULL);
    if (status) { printf("  status %ld\n", status); return; }
    for (r = rec; r; r = r->pNext)
        if (r->wType == DNS_TYPE_A)
        {
            struct in_addr a;
            a.s_addr = r->Data.A.IpAddress;
            printf("  A %s ttl=%lu\n", inet_ntoa(a), r->dwTtl);
        }
    DnsRecordListFree(rec, DnsFreeRecordList);
}

/* The HTTP path the Steam client takes before its CM connection: WinHTTP
 * proxy auto-detection, then ISteamDirectory/GetCMListForConnect over
 * HTTPS. Prints the elapsed time and the result of each step. */
static void print_winhttp(void)
{
    static const WCHAR host[] = L"api.steampowered.com";
    static const WCHAR path[] = L"/ISteamDirectory/GetCMListForConnect/v1/?cellid=0&qoslevel=3";
    WINHTTP_AUTOPROXY_OPTIONS opts;
    WINHTTP_PROXY_INFO proxy;
    HINTERNET session, conn = NULL, req = NULL;
    DWORD t0, status = 0, size = sizeof(status), read = 0;
    char body[256];
    BOOL ok;

    printf("== WinHTTP proxy detection and GetCMListForConnect\n");
    session = WinHttpOpen(L"netprobe", WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0);
    if (!session) { printf("  WinHttpOpen error %lu\n", GetLastError()); return; }
    memset(&opts, 0, sizeof(opts));
    opts.dwFlags = WINHTTP_AUTOPROXY_AUTO_DETECT;
    opts.dwAutoDetectFlags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
    memset(&proxy, 0, sizeof(proxy));
    t0 = GetTickCount();
    ok = WinHttpGetProxyForUrl(session, L"https://api.steampowered.com/", &opts, &proxy);
    printf("  WinHttpGetProxyForUrl(auto-detect) -> %s (error %lu) access=%lu proxy=\"%ls\" in %lu ms\n",
           ok ? "ok" : "failed", ok ? 0 : GetLastError(), proxy.dwAccessType, proxy.lpszProxy ? proxy.lpszProxy : L"", GetTickCount() - t0);
    if (proxy.lpszProxy) GlobalFree(proxy.lpszProxy);
    if (proxy.lpszProxyBypass) GlobalFree(proxy.lpszProxyBypass);

    t0 = GetTickCount();
    conn = WinHttpConnect(session, host, INTERNET_DEFAULT_HTTPS_PORT, 0);
    if (conn) req = WinHttpOpenRequest(conn, L"GET", path, NULL, WINHTTP_NO_REFERER, WINHTTP_DEFAULT_ACCEPT_TYPES, WINHTTP_FLAG_SECURE);
    if (!req) { printf("  WinHttpOpenRequest error %lu\n", GetLastError()); goto done; }
    if (!WinHttpSendRequest(req, WINHTTP_NO_ADDITIONAL_HEADERS, 0, WINHTTP_NO_REQUEST_DATA, 0, 0, 0))
    { printf("  WinHttpSendRequest error %lu after %lu ms\n", GetLastError(), GetTickCount() - t0); goto done; }
    if (!WinHttpReceiveResponse(req, NULL))
    { printf("  WinHttpReceiveResponse error %lu after %lu ms\n", GetLastError(), GetTickCount() - t0); goto done; }
    WinHttpQueryHeaders(req, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_HEADER_NAME_BY_INDEX, &status, &size, WINHTTP_NO_HEADER_INDEX);
    WinHttpReadData(req, body, sizeof(body) - 1, &read);
    body[read] = 0;
    printf("  GET https://%ls%ls -> HTTP %lu in %lu ms, first %lu bytes: %.120s\n", host, path, status, GetTickCount() - t0, read, body);
done:
    if (req) WinHttpCloseHandle(req);
    if (conn) WinHttpCloseHandle(conn);
    WinHttpCloseHandle(session);
}

int main(void)
{
    WSADATA wsa;

    setvbuf(stdout, NULL, _IONBF, 0);
    if (WSAStartup(MAKEWORD(2, 2), &wsa)) { printf("WSAStartup failed %d\n", WSAGetLastError()); return 1; }
    printf("netprobe: start\n");
    print_adapters_addresses();
    print_adapters_info();
    print_network_params();
    print_forward_table();
    print_best_interface();
    print_getaddrinfo(DNS_HOST);
    print_getaddrinfo(CM_HOST);
    print_dnsquery();
    print_dns_probes();
    print_udp_connect_trick();
    print_winhttp();
    print_per_address_probes();
    printf("netprobe: end\n");
    WSACleanup();
    return 0;
}
