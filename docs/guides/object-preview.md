# Object Preview

The console can render an object rather than only downloading it. The same
classification governs what [share links](share-links.md) display and what
[embed links](embed-links.md) may serve inline.

## What renders inline

| Media type | Rendered as | Embeddable in `<img>`/`<video>`/`<audio>` |
| --- | --- | --- |
| `image/jpeg`, `image/png`, `image/webp`, `image/gif` | Image | Yes |
| `video/mp4`, `video/webm` | Video | Yes |
| `audio/mpeg`, `audio/ogg`, `audio/wav`, `audio/x-wav`, `audio/webm` | Audio | Yes |
| `application/pdf` | PDF | No |
| `text/plain`, `text/markdown`, `text/csv` | Text | No |
| `application/json` | JSON | No |

Anything else is `Unsupported` and is offered as a download.

## What is deliberately never rendered

These types can carry script or fetch external references, so Record Store refuses to
render them inline. They remain downloadable as attachments.

```text
text/html                       image/svg+xml
application/xhtml+xml           application/xml, text/xml
text/javascript                 application/javascript
application/ecmascript          application/xslt+xml
application/x-shockwave-flash
```

They are listed explicitly in the source rather than falling through to a default, so
the refusal is a decision rather than an accident.

## The declared type is checked against the bytes

A client controls the `Content-Type` it sends. Before anything is rendered, Record
Store corroborates the declared type against the object's leading bytes.

An object uploaded as `image/png` whose content begins with `<html>` is refused rather
than rendered. This is what stops an attacker from getting HTML executed in your
console's origin by mislabelling it.

## Text previews are bounded

Text and JSON previews read at most `sharing.preview_text_limit_bytes` (default 1 MiB).
The console says when it is showing a slice. The stored object is never altered or
truncated.

## Downloads are unchanged

Whatever the object turns out to be, a download is always served with:

```text
Content-Disposition: attachment
X-Content-Type-Options: nosniff
```

So an object that cannot be previewed is never *less* safe than one that can — it is
simply delivered as a file.
