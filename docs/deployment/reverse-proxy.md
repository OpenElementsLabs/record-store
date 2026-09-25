# Reverse Proxy and TLS

Record Store serves plain HTTP. Put a TLS terminator in front of the public endpoints.

## What to expose

| Port | Expose | Why |
| --- | --- | --- |
| 7600 — S3 API | **Yes**, over TLS | Applications and embed links |
| 7602 — Web console | **Yes**, over TLS | Administrators and share links |
| 7601 — Management API | **No** | Unrestricted administrative access |

The console reaches the management API over the internal network, so 7601 does not
need to be reachable from a browser at all.

If you must reach 7601 remotely, use an SSH tunnel or a VPN rather than a public
hostname:

```bash
ssh -L 7601:127.0.0.1:7601 admin@your-server
```

## Requirements

Three things the proxy must get right for the S3 endpoint:

1. **Preserve the `Host` header.** SigV4 signs it. A proxy that rewrites `Host`
   invalidates every signature and produces `SignatureDoesNotMatch` on requests that
   are perfectly correct.
2. **Do not modify the request body.** The signature covers a hash of the payload.
   Buffering is fine; transforming is not.
3. **Allow large bodies.** Default body-size limits in proxies are far smaller than a
   typical upload.

## Nginx

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name storage.example.com;

    ssl_certificate     /etc/letsencrypt/live/storage.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/storage.example.com/privkey.pem;

    client_max_body_size 0;
    proxy_request_buffering off;

    location / {
        proxy_pass http://127.0.0.1:7600;
        proxy_http_version 1.1;

        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        proxy_read_timeout    600s;
        proxy_send_timeout    600s;
        proxy_connect_timeout 30s;
    }
}

server {
    listen 443 ssl;
    http2 on;
    server_name console.example.com;

    ssl_certificate     /etc/letsencrypt/live/console.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/console.example.com/privkey.pem;

    client_max_body_size 0;
    proxy_request_buffering off;

    location / {
        proxy_pass http://127.0.0.1:7602;
        proxy_http_version 1.1;

        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

`client_max_body_size 0` removes the limit; `proxy_request_buffering off` streams
uploads straight through instead of spooling them to disk first.

## Caddy

```caddyfile
storage.example.com {
    reverse_proxy 127.0.0.1:7600
}

console.example.com {
    reverse_proxy 127.0.0.1:7602
}
```

Caddy preserves `Host`, sets the forwarded headers, streams request bodies, and
obtains certificates automatically. There is nothing further to configure.

## Traefik

```yaml
http:
  routers:
    record-store-s3:
      rule: "Host(`storage.example.com`)"
      service: record-store-s3
      tls:
        certResolver: letsencrypt
    record-store-console:
      rule: "Host(`console.example.com`)"
      service: record-store-console
      tls:
        certResolver: letsencrypt
  services:
    record-store-s3:
      loadBalancer:
        servers:
          - url: "http://record-store:7600"
    record-store-console:
      loadBalancer:
        servers:
          - url: "http://console:7602"
```

Traefik passes `Host` through and sets `X-Forwarded-*` by default.

## Client-address headers

Record Store identifies a client from `X-Forwarded-For` **only when the request
arrived from a hop you have named**. Name them:

```toml
[server]
trusted_proxies = ["10.0.0.0/8"]
```

or `RECORD_STORE_SERVER_TRUSTED_PROXIES=10.0.0.0/8,192.168.1.5`. Entries are IP
addresses or CIDR blocks, in either address family.

!!! warning "Behind a proxy, this is not optional"
    The list is **empty by default**, and while it is empty the header is ignored
    entirely: every request is attributed to the socket it arrived on. Behind a
    proxy that socket is the proxy, for every visitor in the world. The
    consequences are concrete:

    - share-password attempt limits and unknown-token probe limits
      ([Sharing Security](../security/sharing-security.md)) apply to all visitors
      together rather than per visitor;
    - every [audit record](../administration/audit-log.md) says the proxy's
      address instead of the caller's.

Name only hops you actually run. Anything you name is trusted to tell Record Store
who the caller is, so naming an address that is not in front of Record Store hands
whoever can reach the listener from there the ability to choose their own identity —
including the identity that rate limits and audit records are written against.

How the header is read, once a hop is trusted:

- the chain is walked **from the right**, discarding entries that are themselves
  trusted hops, and the first remaining entry is the client;
- everything to the left of that entry was written by somebody who could write
  anything, so it is never used;
- a chain that is entirely trusted hops, a malformed value, or an oversized one
  falls back to the address the request arrived from.

Your proxy should still overwrite rather than append whatever header the client sent,
but Record Store no longer depends on it doing so.

## Console cookies

Behind TLS, set:

```bash
RECORD_STORE_CONSOLE_SECURE_COOKIES=true
```

Without it the session cookie is not marked `Secure`. The usual symptom is signing in
successfully and being signed straight back out.

## Share and embed base URLs

Record Store cannot infer its public hostname from behind a proxy, and share and embed
links are absolute URLs. Set both:

```bash
RECORD_STORE_SHARING_SHARE_BASE_URL=https://console.example.com
RECORD_STORE_SHARING_EMBED_BASE_URL=https://storage.example.com
```

Different hosts on purpose: a share link is a page a person opens on the console, and
an embed serves object bytes from the storage endpoint. Without these, links are built
from the listener address and point at `127.0.0.1`.

## Verifying

```bash
# Host is preserved and signing works
AWS_ACCESS_KEY_ID=<your-access-key> \
AWS_SECRET_ACCESS_KEY=<your-secret-key> \
aws --endpoint-url https://storage.example.com s3 ls

# Large uploads are not truncated by a body limit
head -c 100M /dev/urandom > /tmp/large.bin
aws --endpoint-url https://storage.example.com s3 cp /tmp/large.bin s3://uploads/

# The management port is not publicly reachable
curl -sS --max-time 5 https://storage.example.com:7601/health || echo "closed, as intended"
```

More in [Networking and Proxies](../troubleshooting/networking.md).
