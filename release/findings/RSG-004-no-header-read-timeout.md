# RSG-004: No header read timeout: slow clients hold connections indefinitely

| | |
| --- | --- |
| Status | open |
| Severity | medium when exposed directly; low behind the documented TLS terminator |
| Blocks release | no — mitigated by the deployment the docs require |
| Gate | PERF-OVERLOAD, metric `slow_header_connection_hold_seconds` (advisory) |
| Found | 2026-09-23, candidate `0765aee` |

A client that opens the S3 port and trickles one header line per second without
ever finishing its request is kept connected for the whole 75 s the gate
observes; 48 such connections were all still open after 20 s of overload. Each
holds a descriptor and a task, and nothing bounds how many a client may open,
so a direct exposure can be exhausted with little traffic.

`docs/deployment/reverse-proxy.md` requires a TLS terminator in front of the
public endpoints, and common proxies bound this themselves (nginx
`client_header_timeout`, 60 s default), which is why the metric is advisory.
The fix is a header read timeout on both listeners (hyper's
`header_read_timeout` with a timer), after which the metric can become blocking.
