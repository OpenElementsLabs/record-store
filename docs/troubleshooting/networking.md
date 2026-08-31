# Networking and Proxies

## `SignatureDoesNotMatch` through a proxy

SigV4 signs the `Host` header. A proxy that rewrites it invalidates every signature.

```nginx
proxy_set_header Host $host;
```

Caddy and Traefik preserve `Host` by default; Nginx does not unless told to.

Confirm the diagnosis by bypassing the proxy:

```bash
AWS_ACCESS_KEY_ID=<your-access-key> \
AWS_SECRET_ACCESS_KEY=<your-secret-key> \
aws --endpoint-url http://127.0.0.1:7600 s3 ls
```

## Uploads truncated or refused by size

A proxy body limit. See [Upload Problems](uploads.md#uploads-fail-above-a-size-threshold).

## Slow or hanging large transfers

Raise proxy timeouts and stop buffering requests to disk:

```nginx
proxy_read_timeout 600s;
proxy_send_timeout 600s;
proxy_request_buffering off;
```

## CORS

A browser blocks the request before you ever see it in Record Store's logs.

```bash
aws --endpoint-url https://storage.example.com \
  s3api put-bucket-cors --bucket uploads \
  --cors-configuration file://cors.json
```

```json
{
  "CORSRules": [
    {
      "AllowedOrigins": ["https://app.example.com"],
      "AllowedMethods": ["GET", "PUT", "POST", "HEAD"],
      "AllowedHeaders": ["*"],
      "ExposeHeaders": ["ETag"],
      "MaxAgeSeconds": 3000
    }
  ]
}
```

Points that catch people out:

- **Origins are exact.** `https://app.example.com` does not match
  `https://www.app.example.com`, and scheme and port are part of the origin.
- **`ExposeHeaders: ["ETag"]`** is required for browser-side multipart uploads.
- **Preflight is `OPTIONS`.** A proxy that drops or intercepts `OPTIONS` breaks CORS
  regardless of the bucket configuration.

Check the preflight directly:

```bash
curl -i -X OPTIONS https://storage.example.com/uploads/test.txt \
  -H "Origin: https://app.example.com" \
  -H "Access-Control-Request-Method: PUT"
```

## Share and embed links point at `127.0.0.1`

Record Store cannot infer its public hostname from behind a proxy.

```bash
RECORD_STORE_SHARING_SHARE_BASE_URL=https://console.example.com
RECORD_STORE_SHARING_EMBED_BASE_URL=https://storage.example.com
```

Two different hosts: a share link is a page on the console, an embed serves bytes from
the storage endpoint.

Both must be absolute `http://` or `https://` URLs, no whitespace, under 512 bytes.

## Rate limits apply to everyone at once

Share password attempts and unknown-token probes are limited per client, and the client
is identified from the first entry of `X-Forwarded-For`. Without that header, every
visitor behind the proxy shares one counter.

```nginx
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
```

Have the proxy **overwrite** rather than append any header the client sent — the value
is attacker-influenced.

## Console cannot reach the management API

The console's server calls it; the browser never does.

```bash
RECORD_STORE_API_URL=http://record-store:7601
```

Use the internal service or host name, not `localhost` — in a container, `localhost` is
the console's own container.

## Console signs you straight out

```bash
RECORD_STORE_CONSOLE_SECURE_COOKIES=true
```

Required behind TLS. Without it the session cookie is not marked `Secure`.

## Management port reachable from outside

It must not be.

```bash
curl -sS --max-time 5 https://storage.example.com:7601/health || echo "closed, as intended"
```

If it answers, fix it now: bind it to loopback, or firewall it.

```bash
RECORD_STORE_API_BIND=127.0.0.1:7601
```

To reach it remotely, tunnel:

```bash
ssh -L 7601:127.0.0.1:7601 admin@your-server
```

## TLS certificate errors from clients

- Is the full chain served? Many clients are stricter than browsers about intermediates.
- Does the certificate name match the endpoint the client uses?
- Is the certificate current?

```bash
openssl s_client -connect storage.example.com:443 -servername storage.example.com </dev/null
```

## A reverse proxy configuration that works

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

        proxy_read_timeout 600s;
        proxy_send_timeout 600s;
    }
}
```

Full configurations for Nginx, Caddy, and Traefik are in
[Reverse Proxy and TLS](../deployment/reverse-proxy.md).
