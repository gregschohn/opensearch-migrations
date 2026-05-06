"""Helpers for workflow output artifacts stored in S3."""

import logging
import os
from pathlib import Path
from typing import Dict, List, Optional

import boto3
from botocore import UNSIGNED
from botocore.config import Config
from botocore.exceptions import BotoCoreError, ClientError

logger = logging.getLogger(__name__)

ARTIFACT_BUCKET_ENV = "S3_ARTIFACTS_BUCKET_NAME"
ARTIFACT_REGION_ENV = "S3_ARTIFACTS_REGION"
ARTIFACT_ENDPOINT_ENV = "S3_ARTIFACTS_ENDPOINT_URL"
ARTIFACT_MOUNT_ENV = "S3_ARTIFACTS_MOUNT_POINT"
DEFAULT_ARTIFACT_MOUNT = "/s3/artifacts"


class ArtifactStoreError(RuntimeError):
    """Raised when an artifact operation cannot be completed."""


def _artifact_bucket() -> Optional[str]:
    return os.getenv(ARTIFACT_BUCKET_ENV) or os.getenv("BUCKET_NAME")


def _artifact_client():
    endpoint_url = os.getenv(ARTIFACT_ENDPOINT_ENV)
    region_name = os.getenv(ARTIFACT_REGION_ENV) or os.getenv("AWS_REGION")
    config = Config(signature_version=UNSIGNED) if endpoint_url else None
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url or None,
        region_name=region_name or None,
        config=config,
    )


def _mounted_artifact_path(s3_key: str) -> Path:
    mount = Path(os.getenv(ARTIFACT_MOUNT_ENV, DEFAULT_ARTIFACT_MOUNT))
    return mount / s3_key


def read_artifact_text(s3_key: str) -> str:
    """Read an artifact by S3 key, preferring the console's mounted artifact bucket."""
    mounted_path = _mounted_artifact_path(s3_key)
    if mounted_path.exists():
        return mounted_path.read_text(encoding="utf-8", errors="replace")

    bucket = _artifact_bucket()
    if not bucket:
        raise ArtifactStoreError(f"{ARTIFACT_BUCKET_ENV} is not configured")

    try:
        response = _artifact_client().get_object(Bucket=bucket, Key=s3_key)
        return response["Body"].read().decode("utf-8", errors="replace")
    except (BotoCoreError, ClientError) as e:
        raise ArtifactStoreError(f"could not read s3://{bucket}/{s3_key}: {e}") from e


def artifact_uri(prefix_or_key: str) -> str:
    bucket = _artifact_bucket()
    return f"s3://{bucket}/{prefix_or_key}" if bucket else prefix_or_key


def list_artifacts(prefix: str) -> List[Dict[str, object]]:
    """List artifact objects under a prefix."""
    bucket = _artifact_bucket()
    if bucket:
        try:
            s3 = _artifact_client()
            paginator = s3.get_paginator("list_objects_v2")
            objects = []
            for page in paginator.paginate(Bucket=bucket, Prefix=prefix):
                for item in page.get("Contents", []):
                    objects.append({
                        "key": item["Key"],
                        "last_modified": item.get("LastModified"),
                        "size": item.get("Size", 0),
                    })
            return objects
        except (BotoCoreError, ClientError) as e:
            raise ArtifactStoreError(f"could not list s3://{bucket}/{prefix}: {e}") from e

    mounted_prefix = _mounted_artifact_path(prefix)
    if not mounted_prefix.exists():
        return []
    return [
        {
            "key": str(path.relative_to(_mounted_artifact_path(""))),
            "last_modified": path.stat().st_mtime,
            "size": path.stat().st_size,
        }
        for path in mounted_prefix.rglob("*")
        if path.is_file()
    ]


def delete_artifact_prefix(prefix: str) -> int:
    """Delete all objects under a prefix. Returns the number of deleted keys."""
    bucket = _artifact_bucket()
    if not bucket:
        logger.debug("%s is not configured; skipping artifact cleanup for %s", ARTIFACT_BUCKET_ENV, prefix)
        return 0

    try:
        s3 = _artifact_client()
        paginator = s3.get_paginator("list_objects_v2")
        deleted = 0
        for page in paginator.paginate(Bucket=bucket, Prefix=prefix):
            objects = [{"Key": item["Key"]} for item in page.get("Contents", [])]
            for start in range(0, len(objects), 1000):
                chunk = objects[start:start + 1000]
                if chunk:
                    s3.delete_objects(Bucket=bucket, Delete={"Objects": chunk})
                    deleted += len(chunk)
        return deleted
    except (BotoCoreError, ClientError) as e:
        raise ArtifactStoreError(f"could not delete s3://{bucket}/{prefix}: {e}") from e
