# RSG-008: Storage-event pagination drops one event at every page boundary

| | |
| --- | --- |
| Status | open |
| Severity | high — "a committed change always produces its event" does not hold for anyone who pages |
| Blocks release | yes |
| Gate | COR-PAGINATION, REC-CRASH |
| Known failing checks | `storage events at limit=`; `storage events filtered by prefix`; `the paginated storage-event walk returns every event` |
| Found | 2026-09-23, candidate `417091a` (product code as `0765aee`), first on the GitHub-hosted Linux runner |

Walking `GET /api/v1/events` with the documented cursor (`after_time` and
`after_id` from the previous page's `next_time` and `next_id`) loses exactly one
event per page boundary, with or without filters:

| limit | pages | events missing |
| --- | --- | --- |
| 1000 | 2 | 1 |
| 500 | 3 | 2 |
| 97 | 16 | 15 |
| 7 | 188 | 187 |

Every timestamp in the set was distinct, so this is not a tie-breaking case.
The audit trail's identical-looking cursor is not affected (COR-PAGINATION).
A webhook subscriber is not affected either: delivery does not page this API.

## Reproduction

```bash
out/venv/bin/python tests/gates/pagination.py --bin-dir out/candidate/bin
# [FAIL] storage events at limit=97 return every event once -- {'missing': 12, ...}
```

REC-CRASH first showed it on Linux, where faster small writes put more than 500
events in one bucket: an event written before any crash was missing after the
second one, yet present when looked up by its exact key.

## Cause

`crates/record-store-events/src/lib.rs`, `list_events`: when a page is full the
loop sets `page.next` to the *first event not returned*, and the next request
uses the cursor as the exclusive upper bound of the scan
(`lower..event_time_key(time, id)`), so that event is never returned. The cursor
should name the last event returned.
