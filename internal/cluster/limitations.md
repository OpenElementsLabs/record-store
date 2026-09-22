# Cluster limitations and open risks

Internal. Not published. See [`../README.md`](../README.md).

This is the honest list. Anything here is a reason clustering is not publicly
documented yet. Items are ordered by how much they would hurt.

## Blocking

### L1. No multi-process or multi-host test coverage

Every cluster test runs in one process. The internal RPC surface is exercised
over a real listener with the production client, and consensus runs as a real
multi-member group, but no test starts several `record-store-server` processes,
and none runs across hosts.

**What this hides.** Process supervision, startup ordering, real socket
behaviour, partial writes across a real network, host clock differences, disk
behaviour under real concurrent load, and every interaction with the operating
system's scheduler. In-process tests are necessary and not sufficient.

**Until fixed:** no claim about behaviour under real deployment conditions is
supported by evidence.

### L2. No tested recovery from loss of metadata quorum

If a majority of metadata voters is permanently lost, there is no supported
procedure to reconstruct authority. Payload bytes survive and are readable from
disk; the catalog that says what they mean does not.

Deliberately, the system does **not** try to recover automatically. It will not
promote a surviving minority, and it will not pick the longest log as
authoritative. Both would invent authority.

**Mitigation today:** run an odd voter count across failure domains, and take
regular catalog snapshots off-box. **Missing:** a documented, tested restore of a
cluster catalog from an external backup, including how object history, retention,
and cluster identity are preserved.

### L3. Asymmetric partitions are untested

The consensus test harness models reachability symmetrically: if A cannot reach
B, B cannot reach A. Real networks produce one-way partitions, and they are a
classic source of election churn and stuck leaders.

**Risk:** a leader that can send but not receive, or a follower that can be
appended to but cannot vote, may produce behaviour no test covers. openraft's own
correctness is not in question; Record Store's timeouts, readiness reporting, and
coordination fencing on top of it are.

### L4. No concurrent-history linearizability checking

Read consistency is claimed as linearizable and is implemented with read
barriers, with tests that a leader's commit is visible to a follower on the first
read. What does not exist is a harness that records invocation and completion
histories under concurrency and checks them against the model, distinguishing
unavailable, failed, and ambiguous outcomes.

**Risk:** the barrier is applied on the object path today; a future path added
without one would not be caught by any existing test.

## Significant

### L5. Storage fault injection is missing

No test covers a full disk, a disk that returns errors partway through a write,
a disk that is pathologically slow, or a filesystem that lies about `fsync`.
Repair bandwidth and concurrency are bounded by configuration, but the bounds
have not been validated against a genuinely slow device.

### L6. No soak or long-outage testing

Retries, pending requests, and task growth are bounded in code — per-chunk
deadlines on writes, leases on movement tasks, attempt budgets, per-device
movement caps. None of it has been run for hours against a prolonged outage, so
resource growth over time is argued rather than measured.

### L7. Repair source selection trusts an authenticated peer's bytes at the
source, and verifies at the destination

The destination recomputes the checksum over what it wrote and refuses a
mismatch, so corrupt bytes are never promoted. But a peer that is authenticated
and *deliberately* malicious is outside the failure model (§1 of the failure
model). This is stated, not solved.

### L8. Coordination scan cursor resets on leadership change

The repair scan walks placements incrementally with an in-memory cursor. A
leadership change restarts the new leader's cursor at the beginning. Work is not
lost — repair is re-derived from replicated state — but a cluster that changes
leader frequently rescans the early range more often than the late one, so the
time to notice a durability deficit in the late range is worse than the scan
interval suggests.

### L9. Movement fencing narrows a window rather than closing it

A replica movement re-checks its claim against committed state immediately
before it releases the source replica — the only step that destroys bytes. The
check is a barriered read, so it is authoritative, but it is not atomic with the
release: a lease that expires in the microseconds between the check and the
delete would not be caught.

Closing it completely would require the release itself to be conditional on the
fence, which means the delete and the placement change committing together. That
is a larger change than the window justifies today, and the window is bounded by
the lease (default 600 s) rather than by network timing.

### L10. Forced removal reports a deficit but does not verify it afterwards

`decommission --force` reports how many object versions drop below required
durability, based on committed placement at the time of the check. Nothing
re-verifies that the reported deficit matched reality once removal completed.

## Accepted, with reasons

### L11. Event delivery is at-least-once

The intent to publish is committed with the mutation, and the identifier is now
derived deterministically from the event's content, so a republish after a crash,
failover, or snapshot install carries the same identifier. Subscribers must still
deduplicate: delivery can repeat, and exactly-once is not offered.

### L12. Retries of an ambiguous write can create a second version

If a client times out on an ambiguous outcome and retries against a versioned
bucket, it can create a second version. This matches S3's own behaviour. The
guarantee Record Store adds is that the *first* version is never destroyed by the
retry — see the ambiguous-commit handling in the failure model.

### L13. Pre-fencing movement tasks are refused rather than migrated

A replica movement task written before fence tokens existed decodes with token
zero, which no claim ever issues, so it cannot report an outcome. Its lease
expires and it is reclaimed and re-issued a valid token. This costs one lease
period on upgrade and needs no migration.
