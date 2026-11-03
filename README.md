# wolos-cdn

The WolOS package bucket on Square Cloud. `GET /<path>` serves a file; a missing one is pulled once from
`ORIGIN` (GitHub release downloads, `<repo>-<date>/<file>`), kept on disk when it never changes
(packages, ISOs) and passed through when it does (databases, manifests, signatures).
`PUT`/`DELETE /<path>` with `Authorization: Bearer $UPLOAD_TOKEN` change files; writes are atomic.
Files over 100 MB (the hosting proxy's cap on one request) go up in parts: see [Chunked upload](#chunked-upload).

| env | default | |
|---|---|---|
| `PORT` | 80 | listen port |
| `DATA_DIR` | `data` | bucket directory |
| `UPLOAD_TOKEN` | unset (read only) | set in the Square Cloud dashboard, never in this repo |
| `ORIGIN` | unset | base URL for pull-through |
| `MIRRORS` | unset | `name=url,...`: `/name/x` pulls `url/x` |
| `MAX_BYTES` | 7 GiB | disk budget; least recently used packages and ISOs go first, never the newest ISO (outside `MIRRORS` prefixes) or its `.sha256`/`.sig`; also the largest chunked upload |
| `KEEP_ISOS` | 1 | after an `*.iso` upload, ISOs kept in its directory, newest first; older ones go with their `.sha256`/`.sig` (`x.iso.sha256` or `x.sha256`); the top-level `v<semver>/` folders count as one directory (highest versions stay, and the ISO just stored); 0 keeps all |
| `REPORTS_DIR` | beside `DATA_DIR` | error reports |

## Releases

The primary download channel. A release is a top-level `v<semver>/` folder (`v0.12.0/`) holding the
ISO, its `x.iso.sha256` (`sha256sum` format) and `release.json`. Upload them in that order,
`release.json` last: a folder with an ISO but no checksum is left out until it is complete.

| request | answers |
|---|---|
| `GET /releases.json` | `{"releases":[{version, date, iso, size, sha256, release_json, notes_url, mirror}]}`, newest version first (semver) |
| `GET /releases/latest.json` | the first entry (404 when there is none) |
| `GET /releases` | the download page: links, sizes, sha256 with a verify line, how to write the USB stick (linked from `/`) |

`version` is the folder name without `v`; `iso` the file name in the folder (download
`/v<version>/<iso>`); `release_json` its path here; `date` and `notes_url` come from release.json
(`date` falls back to the ISO's file time, `YYYY-MM-DD` UTC); `mirror` is release.json's `image`
(the GitHub asset). Once `KEEP_ISOS` or the disk budget removed the ISO, the version stays listed
with `iso` null, `size` and `sha256` from release.json, and `mirror` as the download. The JSON is
`no-cache` with `Access-Control-Allow-Origin: *`. The index is kept in memory and built again after
any upload, delete or eviction under a `v*/` folder.

## Chunked upload

All requests carry `Authorization: Bearer $UPLOAD_TOKEN`. A session lives in
`DATA_DIR/.uploads/<sha256 of path>/` (the parts and a JSON manifest: path, total, received ranges,
start time) and is never served. Paths follow the usual rules (no `..`, no `.upload*` names).

| request | does | answers |
|---|---|---|
| `PUT /<path>` + `Content-Range: bytes a-b/total` | stores bytes a..b (inclusive) of a `total`-byte file; any order; the same range again overwrites it | 202 `{path,total,received,ranges,missing}` |
| `POST /<path>?complete=1` + `X-Content-Sha256: <hex>` | joins the parts in order, checks length and digest, renames into place; ISOs then follow `KEEP_ISOS` | 201 `{path,size,sha256,removed}` |
| `DELETE /<path>?upload=1` | drops the session | 200 `{path,aborted}` |

Refusals are JSON `{"error": ...}`: 400 bad `Content-Range`, body length or digest header; 401 token;
404 no session; 409 range overlaps a different received range, total differs from the session's,
parts missing (with `missing`), or already completing; 413 part over 100 MiB or total over
`MAX_BYTES`; 416 range outside the total; 422 joined length or sha256 wrong. A complete that fails
drops the session (the parts are consumed while joining): start again. Sessions older than 24 h are
swept (at start, hourly, and when a new one opens). `PUT` without `Content-Range` still writes small
files in one request.

```sh
curl -H "Authorization: Bearer $UPLOAD_TOKEN" -H "Content-Range: bytes 0-94371839/2147483648" \
     -T part0 https://wolos.squareweb.app/iso/wolos-0.12.0-x86_64.iso
# ... the other parts, then:
curl -X POST -H "Authorization: Bearer $UPLOAD_TOKEN" -H "X-Content-Sha256: $(sha256sum wolos.iso | cut -c1-64)" \
     "https://wolos.squareweb.app/iso/wolos-0.12.0-x86_64.iso?complete=1"
```

The client does all of it: `tools/cdn-put` (POSIX sh, curl) cuts the file into parts of up to
90 MiB, sends 4 at a time, retries each (no answer, 408, 429, 5xx) with growing pauses, aborts the
session on failure and completes with the file's sha256. It prints the server's JSON on success.

```sh
UPLOAD_TOKEN=... tools/cdn-put out/wolos.iso iso/wolos-0.12.0-x86_64.iso [--base https://wolos.squareweb.app]
```

| env | default | |
|---|---|---|
| `CDN_PUT_PART_MIB` | 90 | part size in MiB (1 to 95) |
| `CDN_PUT_JOBS` | 4 | parts in flight |
| `CDN_PUT_TRIES` | 5 | tries per request |

## Error reports

WolOS machines that turn reports on send `POST /report` (a JSON object with `kind`, at most 16 KiB;
no client address is kept). The status page and `/stats` show today's counts by kind. Read them:

```sh
curl -H "Authorization: Bearer $UPLOAD_TOKEN" "https://wolos.squareweb.app/report?days=7"
```

Kept 30 days, at most 20 000 a day.

Deployed on Square Cloud from the `Dockerfile` (settings in the dashboard; set `UPLOAD_TOKEN` there). `cargo test` runs the server end to end.
