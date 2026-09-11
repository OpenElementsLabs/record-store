# Security Policy

Record Store stores other people's data. A vulnerability here is a vulnerability in
whatever a deployment holds, so we would rather hear about it early and privately than
read about it in a public issue.

This file is about **reporting a flaw in Record Store**. For running a deployment
securely — authentication, authorization, encryption, sharing, and the hardening
checklist — see the [Security documentation](https://openelementslabs.github.io/record-store/security/),
in this repository under [`docs/security/`](docs/security/).

## Supported versions

Record Store is pre-1.0. Only the most recent released version receives security
fixes, and fixes ship in a new release rather than as backports to an earlier line.

| Version | Supported |
| --- | --- |
| 0.1.1 (latest release) | ✅ |
| 0.1.0 | ❌ — upgrade to the latest release |
| `main` (unreleased) | ✅ — report it; the fix lands here first |

Container images are published per release to the GitHub Container Registry. Running
an image older than the latest release means running without published security fixes.

## Reporting a vulnerability

**Do not open a public issue, pull request, or discussion for a security problem.**

Report it through GitHub's private vulnerability reporting:

**[Report a vulnerability](https://github.com/OpenElementsLabs/record-store/security/advisories/new)**
— or the *Report a vulnerability* button on the repository's
[Security tab](https://github.com/OpenElementsLabs/record-store/security).

That opens a private advisory visible only to you and the maintainers. It is not
indexed, and nothing becomes public until we publish an advisory.

If you cannot use GitHub private reporting, open a public issue that says only that you
have a security report and asks a maintainer to contact you — **no details** — and we
will move the conversation somewhere private.

### What to include

A report we can reproduce gets fixed considerably faster than one we cannot. Where
you can, include:

- The version, commit, or image digest you tested.
- Deployment mode (single node or cluster) and the relevant configuration, with
  secrets removed.
- Which plane is affected: the S3 data plane (port 7600), the management plane
  (port 7601), the console, or a share or embed link.
- Steps to reproduce, ideally a request sequence or a short script.
- What an attacker gains — data read, data written, privilege gained, availability
  lost — and what access they need to start.

### What to expect

| | |
| --- | --- |
| Acknowledgement | Within 3 business days |
| Initial assessment | Within 10 business days — whether we can reproduce it, and severity |
| Progress updates | At least every 10 business days while the report is open |
| Fix and advisory | Coordinated with you before anything is published |

We publish a GitHub Security Advisory when a fix is released, and credit the reporter
by name or handle unless you ask us not to. If a report turns out not to be a
vulnerability, we will say so and explain why rather than leaving it open.

Please give us a reasonable chance to ship a fix before disclosing publicly. We are
not going to sit on a report — but a fix that reaches deployments before the details
do is the whole point.

## Scope

In scope — anything in this repository:

- The Rust workspace: the server, the CLI, and every crate under `crates/`.
- The Next.js console under `console/`.
- Published container images and the release artifacts that accompany them.
- Deployment material under `deploy/`, and the example configuration.
- Documentation that recommends a genuinely insecure configuration.

Particularly interesting: authentication or authorization bypass on either plane,
policy evaluation that allows what it should deny, capability or share tokens that
authorize more than the object they were issued for, secrets appearing in a log or an
API response, audit records that can be altered or suppressed, and anything reachable
before authentication.

Out of scope:

- Vulnerabilities in a third-party dependency with no exploitable path through
  Record Store — report those upstream. If there *is* a path through Record Store,
  it is in scope; tell us.
- A deployment configured against the documented guidance, such as running the
  management plane on a public network, or reusing default credentials. See the
  [Security Checklist](https://openelementslabs.github.io/record-store/security/checklist/).
- Findings from an automated scanner with no demonstrated impact.
- Denial of service through sheer volume of traffic.
- Missing hardening headers or similar findings with no demonstrated impact.

## Testing

Test against your own deployment. Do not test against a deployment you do not
own or do not have written permission to test, and do not access, modify, or retain
anyone else's data while investigating.
