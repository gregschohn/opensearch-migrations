# Replay Tuple Output

The traffic replayer writes one result tuple per replayed request, with the source request and response
alongside the target's. Tuples can be persisted to S3 as gzip-compressed JSON lines and inspected from
the Migration Console.

## Inspecting from the Migration Console

Run this from a Migration Console shell. When more than one replayer exists, add `--replayer <name>`
to choose one.

```
console tuples show --limit 5 --pretty
```

## Configuration

Set `tupleOutput.repoPathUri` to an S3 URI to persist replay result tuples, or select `type: disabled`
to use the configured tuple logger without S3 persistence. Omitting `tupleOutput` also uses the
configured tuple logger.

> **Upgrading:** the tuple output configuration under `replayerConfig` has changed. Replace
> `tupleS3Bucket`, `tupleS3Prefix`, `tupleS3Region`, and `tupleS3Endpoint` with a `tupleOutput` object.
> Combine the bucket and prefix in `repoPathUri: s3://<bucket>/<prefix>`, and use `awsRegion` and
> `endpoint` for the region and endpoint. The former flat fields are no longer accepted.

The destination is set under a replayer's `traffic.replayers.<name>.replayerConfig`:

```yaml
replayerConfig:
  tupleOutput:
    repoPathUri: s3://my-tuple-results-bucket/tuples/
    awsRegion: us-east-1
```

or, to opt out:

```yaml
replayerConfig:
  tupleOutput:
    type: disabled
```

Changing the tuple output bucket on a running replay needs `workflow approve change`.

| Option                    | Default | Description                                                       |
| ------------------------- | ------- | ----------------------------------------------------------------- |
| `tupleOutput.repoPathUri` | none    | Required for S3 persistence; `s3://<bucket>/<prefix>`.            |
| `tupleOutput.awsRegion`   | none    | Required for S3 persistence; region for the bucket.               |
| `tupleOutput.endpoint`    | none    | HTTP(S) or LocalStack endpoint override.                          |
| `tupleOutput.type`        | none    | Use `disabled` to opt out; cannot be combined with URI fields.    |
| `tupleMaxBufferSeconds`   | `60`    | Max seconds before rotating and uploading a tuple file.           |
| `tupleMaxFileSizeMb`      | `256`   | Max uncompressed file size in MB before rotating to a new object. |
| `tupleMaxPerFile`         | `0`     | Max tuples per object. `0` disables the count limit.              |

The rotation settings belong directly to `replayerConfig`, alongside `tupleOutput`.
The region must be set explicitly for an S3 destination.

The URI supplies the bucket and prefix:

| URI                                    | Key prefix          |
| -------------------------------------- | ------------------- |
| `s3://my-tuple-results-bucket`         | `tuples/` (default) |
| `s3://my-tuple-results-bucket/`        | empty (bucket root) |
| `s3://my-tuple-results-bucket/custom/` | `custom/`           |

The location uses the same `repoPathUri`, `awsRegion`, and `endpoint` fields as snapshot repositories
and the [failed-document stream](failedDocumentStream.md). Paths are preserved literally.
LocalStack endpoints support `localstack://` and `localstacks://` and are resolved to HTTP(S) IP
endpoints before submission.

Tuple persistence supports S3. Native GCS and Azure Blob Storage destinations are not accepted.
