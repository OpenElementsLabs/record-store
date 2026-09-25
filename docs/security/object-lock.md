# Object Lock and Trust

Object Lock is a retention *control*, not a proof. This page is about the difference,
because the gap between the two is where people get hurt.

For how to use the feature, see [Object Lock](../administration/object-lock.md).

## What a retention date actually proves

A `COMPLIANCE` retention on a Record Store version means: **this deployment's software
refuses to delete or shorten it, for anyone, through any interface it exposes.** That
refusal is real, it is enforced inside the metadata transaction that would do the
removal, and it applies to the root credential and to the management API as much as to an
ordinary service account.

That is a genuine and useful property. It is also the whole of it.

## What it does not prove

Record Store is a single process with one copy of your data on disks you control. It
follows that:

- **An operator with filesystem access can delete the payload.** Object Lock is enforced
  by Record Store. It is not enforced by the filesystem, and Record Store does not make
  the disk refuse writes. Someone with `rm` and the data directory does not go through
  the code that would refuse them.
- **An operator with filesystem access can edit the retention.** Lock state lives in the
  metadata catalog. It is not signed, and signing it here would not help: the key would
  have to be derivable by the same process, from a master key in the same environment.
  Anyone who can rewrite the catalog can generally also read that environment.
- **A retention date is not a timestamp.** It records when this deployment intends to
  stop retaining an object. It does not establish when the object was written, and it is
  not evidence to a third party who does not already trust the deployment.
- **Object Lock is not backup.** It refuses deletions; it does not survive losing the
  disk. See [Durability](../concepts/durability.md).

If your threat model includes the person who administers the server, Object Lock alone
does not cover it. Saying otherwise would be selling a property the software does not
have.

## Why we say this plainly

Retention features are routinely described in ways that imply resistance to a hostile
administrator. For a single-node, self-hosted service storing data on an operator's own
disks, that implication is false, and an operator who believes it will build a compliance
story on a foundation that does not hold.

What Object Lock genuinely gives you is protection against the far more common failures:
the script with the wrong prefix, the application deleting what it should have archived,
the lifecycle rule that was more aggressive than intended, the operator who meant to
delete last quarter's data. Those are worth preventing, and Object Lock prevents them.

## The clock

Retention is a comparison against the current time, so it is only as trustworthy as the
clock making that comparison. A clock that jumps forward makes a retention expire early;
one that is simply wrong makes every date meaningless.

Record Store persists a **monotonic high-water mark** of the furthest point in wall-clock
time it has ever observed. The mark is refreshed on a timer as well as by lock
operations, so a deployment that sits idle for a month still notices a clock that moved
while nothing was happening.

When the clock reports a time behind that mark, beyond a configured tolerance:

- Operations that would **release** something — deleting a retained version, shortening
  or removing a retention, removing a legal hold, exercising a governance bypass,
  lifecycle expiry of a locked version — are refused with `503 ServiceUnavailable`, and a
  warning is logged.
- Operations that only **add** protection, and ordinary reads and writes, keep working.
  Getting those wrong can only ever over-retain, which is the safe direction.

Record Store deliberately does **not** substitute the high-water mark for the wall clock
when the clock is behind. Doing so would mean a single bogus forward jump became a
permanent licence to delete early. Refusing is recoverable; releasing is not.

The limits of this, stated plainly:

- It detects a clock moving **backwards** past a point already seen. It cannot detect a
  clock that was wrong from the start, or one that only ever runs fast.
- The high-water mark lives in the same catalog as everything else, so it offers nothing
  against someone who can edit that catalog.
- The tolerance exists to absorb ordinary NTP correction. It is capped at 300 seconds,
  because a tolerance wide enough to hide a meaningful jump would make the mark
  decorative.

Run NTP. The mark is a backstop for a clock that misbehaves, not a substitute for one
that works.

## Getting closer to proof

Two things genuinely raise the bar, and neither is Object Lock:

- **Off-host copies.** A replica an operator of this host cannot reach is the only thing
  that survives that operator. Object Lock protects a copy; it does not create one.
- **External anchoring.** A record of the deployment's state committed somewhere outside
  it, at a time a third party can verify, is what makes a past state provable against
  someone with full local access. Record Store does not do this yet.

Until then: Object Lock means Record Store will refuse. It does not mean the bytes cannot
be removed by someone who owns the machine.

## Operational checklist

- [ ] Choose `COMPLIANCE` only where you accept that **nothing** releases it early —
      not the root credential, not a support escalation, not you.
- [ ] Grant `s3:BypassGovernanceRetention` separately from `s3:DeleteObjectVersion`, and
      to as few principals as possible.
- [ ] Review bypass audit records: `record-store audit | grep object-lock.bypass`. Each
      bypass leaves an `attempted` record and an outcome; an `attempted` with no outcome
      is an override this server cannot account for and is worth investigating.
- [ ] Recheck the audit chain periodically:
      `record-store audit-export verify-chain`. It detects a record edited or removed by
      anyone who could not also rewrite every later link — which is not the same as
      detecting the operator of this host. See
      [Audit Log](../administration/audit-log.md#checking-the-log-has-not-been-edited).
- [ ] Review lifecycle skip records, so a rule that silently expires nothing is visible.
- [ ] Run NTP, and alert on the clock-behind-high-water-mark warning.
- [ ] Keep backups. Object Lock is not one.
- [ ] Restrict filesystem access to the data directory, and treat that access as
      equivalent to the ability to delete retained records — because it is.
