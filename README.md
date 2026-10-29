# wolos-cdn

The WolOS package bucket on Square Cloud. `GET /<path>` serves a file; a missing one is pulled once from
`ORIGIN` (GitHub release downloads, `<repo>-<date>/<file>`), kept on disk when it never changes
(packages, ISOs) and passed through when it does (databases, manifests, signatures).
`PUT`/`DELETE /<path>` with `Authorization: Bearer $UPLOAD_TOKEN` change files; writes are atomic.

| env | default | |
|---|---|---|
| `PORT` | 80 | listen port |
| `DATA_DIR` | `data` | bucket directory |
| `UPLOAD_TOKEN` | unset (read only) | set in the Square Cloud dashboard, never in this repo |
| `ORIGIN` | unset | base URL for pull-through |
| `MIRRORS` | unset | `name=url,...`: `/name/x` pulls `url/x` |
| `MAX_BYTES` | 7 GiB | disk budget; least recently used packages and ISOs go first |
| `REPORTS_DIR` | beside `DATA_DIR` | error reports |

## Error reports

WolOS machines that turn reports on send `POST /report` (a JSON object with `kind`, at most 16 KiB;
no client address is kept). The status page and `/stats` show today's counts by kind. Read them:

```sh
curl -H "Authorization: Bearer $UPLOAD_TOKEN" "https://wolos.squareweb.app/report?days=7"
```

Kept 30 days, at most 20 000 a day.

Deployed on Square Cloud from the `Dockerfile` (settings in the dashboard; set `UPLOAD_TOKEN` there). `cargo test` runs the server end to end.
