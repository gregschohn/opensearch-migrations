# Replay tuple output

Configure persisted replay result tuples under `traffic.replayers.<name>.replayerConfig.tupleOutput`.
The destination uses the same `repoPathUri`, `awsRegion`, and `endpoint` fields as snapshot repositories
and the [failed-document stream](failedDocumentStream.md). The URI carries both the bucket and prefix.

```yaml
replayerConfig:
  tupleOutput:
    repoPathUri: s3://my-tuple-results/tuples/
    awsRegion: us-east-1
    # endpoint: http://localhost:4566
  tupleMaxBufferSeconds: 60
  tupleMaxFileSizeMb: 256
  tupleMaxPerFile: 0
```

An enabled destination requires `repoPathUri` and an explicit `awsRegion`. Tuple rotation settings
belong to `replayerConfig`.

| URI | Resolved bucket | Resolved prefix |
|---|---|---|
| `s3://my-tuple-results` | `my-tuple-results` | `tuples/` (default) |
| `s3://my-tuple-results/` | `my-tuple-results` | empty (bucket root) |
| `s3://my-tuple-results/custom/` | `my-tuple-results` | `custom/` |

Paths use the same validation as S3 snapshot repository paths and are preserved literally.
The optional endpoint accepts HTTP(S), `localstack://`, and `localstacks://` URLs. LocalStack hosts
are resolved to IP addresses before submission, matching snapshot repository endpoint handling.

Omit `tupleOutput`, or select `tupleOutput: {type: disabled}`, to use the configured tuple logger
without S3 persistence. This differs from `failedDocumentStream`, where an explicit choice is required.

Only `s3://` output is implemented. Snapshot repositories also support GCS and Azure, but their
readers do not provide tuple or failure writers; `gs://` and `azure://` output URIs are rejected.
Snapshot-specific credentials such as `s3RoleArn` and `azureCredentialsSecretName` do not belong
in output configuration.

Replace the former flat user fields as follows:

| Former field | Replacement |
|---|---|
| `tupleS3Bucket` and `tupleS3Prefix` | `tupleOutput.repoPathUri: s3://<bucket>/<prefix>` |
| `tupleS3Region` | `tupleOutput.awsRegion` |
| `tupleS3Endpoint` | `tupleOutput.endpoint` |

The config processor resolves the destination into the existing flat workflow, TrafficReplay CRD,
and replayer options. Direct replayer CLI flags and console tuple inspection keep their existing
interfaces. Destination changes retain the existing workflow approval requirements.
