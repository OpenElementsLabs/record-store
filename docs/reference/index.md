# Reference

Exact values, verified against the implementation.

<div class="grid cards" markdown>

-   **[CLI Reference](cli.md)** — every command and flag
-   **[Configuration Reference](configuration.md)** — every setting and its range
-   **[Environment Variables](environment-variables.md)** — the complete list
-   **[Ports](ports.md)** — what listens where, and what to expose
-   **[S3 Compatibility](s3-compatibility.md)** — what is and is not supported
-   **[Management API](management-api.md)** — routes and shapes
-   **[Error Reference](errors.md)** — codes and what to do about them
-   **[Glossary](glossary.md)** — terms used throughout

</div>

## Quick facts

| | |
| --- | --- |
| S3 API | `7600` |
| Management API | `7601` |
| Web console | `7602` |
| Internal RPC | `7603` |
| Signing | AWS SigV4 only |
| Addressing | Path-style |
| Presign ceiling | 604800 seconds (7 days) |
| Replication factor | 1–3, default 3 |
| Temporary credential lifetime | 60–86400 seconds |
| Join token lifetime | 60–86400 seconds |
| Audit query limit | 1–1000, default 100 |
| Lifecycle expiration | 1–36500 days |

## Not implemented

Documented so you do not go looking:

| | Status |
| --- | --- |
| Erasure coding | Not implemented. Replication is the durability model |
| Multi-region conflict resolution | Not implemented |
| `UploadPartCopy` | Unsupported |
| Server-side encryption request headers | Unsupported — encryption is a deployment setting |
| Access control lists | Unsupported — use [policies](../administration/policies.md) |
| Object Lock | Unsupported |
| `aws-chunked` transfer encoding | Unsupported |
| Session tokens (STS-style) | Not issued. [Temporary credentials](../administration/temporary-credentials.md) are plain key pairs |
