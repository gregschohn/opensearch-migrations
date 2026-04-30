"""
Client for the opensearch-pricing-calculator HTTP API (port 5050).

See: https://github.com/opensearch-project/opensearch-migrations/tree/main/AIAdvisor/opensearch-pricing-calculator

The calculator must be running locally before calling any estimate method.
Start it with:

    go build -o opensearch-pricing-calculator .
    ./opensearch-pricing-calculator

or via Docker:

    docker run -p 5050:5050 -p 8081:8081 opensearch-pricing-calculator

The base URL defaults to ``http://opensearch-pricing-calculator:5050`` and can be overridden via
the ``OPENSEARCH_PRICING_CALCULATOR_URL`` environment variable or the ``base_url``
constructor argument.

Usage::

    client = PricingCalculatorClient()

    # Managed cluster — search workload
    result = client.estimate_provisioned_search(
        size_gb=200, azs=3, replicas=1,
        target_shard_size_gb=25, cpus_per_shard=1.5,
        region="US East (N. Virginia)",
    )

    # Managed cluster — time-series workload
    result = client.estimate_provisioned_time_series(
        size_gb=500, azs=3, replicas=1,
        hot_retention_days=14, warm_retention_days=76,
        target_shard_size_gb=45, cpus_per_shard=1.25,
        region="US East (N. Virginia)",
    )

    # Managed cluster — vector workload
    result = client.estimate_provisioned_vector(
        vector_count=10_000_000, dimensions=768,
        engine_type="hnswfp16", max_edges=16,
        azs=3, replicas=1,
        region="US East (N. Virginia)",
    )

    # Serverless collection
    result = client.estimate_serverless(
        collection_type="timeSeries",
        daily_index_size=10, days_in_hot=1, days_in_warm=6,
        min_query_rate=1, max_query_rate=1, hours_at_max_rate=0,
        region="us-east-1", redundancy=True,
    )
"""

from __future__ import annotations

import json
import os
import socket
import urllib.error
import urllib.request
from typing import Any, Dict, Literal

OPENSEARCH_PRICING_CALCULATOR_URL = os.environ.get(
    "OPENSEARCH_PRICING_CALCULATOR_URL", "http://opensearch-pricing-calculator:5050"
)

DEFAULT_REGION = "US East (N. Virginia)"
PROVISIONED_ESTIMATE_PATH = "/provisioned/estimate"


class PricingCalculatorError(Exception):
    """Raised when the pricing calculator returns an error or is unreachable."""


class PricingCalculatorClient:
    """HTTP client for the opensearch-pricing-calculator API.

    Args:
        base_url: Base URL of the running calculator service.
                  Defaults to ``http://opensearch-pricing-calculator:5050``.
        timeout:  Request timeout in seconds. Defaults to 30.
    """

    def __init__(
        self,
        base_url: str | None = None,
        timeout: int = 300,
    ) -> None:
        self.base_url = (base_url or OPENSEARCH_PRICING_CALCULATOR_URL).rstrip("/")
        self.timeout = timeout

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _post(self, path: str, payload: Dict[str, Any], *, timeout: int | None = None) -> Dict[str, Any]:
        """POST *payload* to *path* and return the parsed JSON response.

        Args:
            timeout: Per-request timeout override. Falls back to ``self.timeout``.

        Raises:
            PricingCalculatorError: On HTTP errors or connection failures.
        """
        effective_timeout = timeout if timeout is not None else self.timeout
        url = f"{self.base_url}{path}"
        data = json.dumps(payload).encode("utf-8")
        req = urllib.request.Request(
            url,
            data=data,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=effective_timeout) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", errors="replace")
            raise PricingCalculatorError(
                f"HTTP {exc.code} from pricing calculator at {url}: {body}"
            ) from exc
        except urllib.error.URLError as exc:
            raise PricingCalculatorError(
                f"Could not reach pricing calculator at {url}. "
                "Make sure it is running on port 5050. "
                f"Details: {exc.reason}"
            ) from exc
        except socket.timeout as exc:
            raise PricingCalculatorError(
                f"Request to pricing calculator at {url} timed out "
                f"after {effective_timeout}s. The service may still be starting."
            ) from exc

    def _get(self, path: str, *, timeout: int | None = None) -> Any:
        """GET *path* and return the parsed JSON response.

        Args:
            timeout: Per-request timeout override. Falls back to ``self.timeout``.
        """
        effective_timeout = timeout if timeout is not None else self.timeout
        url = f"{self.base_url}{path}"
        req = urllib.request.Request(url, method="GET")
        try:
            with urllib.request.urlopen(req, timeout=effective_timeout) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", errors="replace")
            raise PricingCalculatorError(
                f"HTTP {exc.code} from pricing calculator at {url}: {body}"
            ) from exc
        except urllib.error.URLError as exc:
            raise PricingCalculatorError(
                f"Could not reach pricing calculator at {url}. "
                "Make sure it is running on port 5050. "
                f"Details: {exc.reason}"
            ) from exc
        except socket.timeout as exc:
            raise PricingCalculatorError(
                f"Request to pricing calculator at {url} timed out "
                f"after {effective_timeout}s. The service may be overloaded or unreachable."
            ) from exc

    # ------------------------------------------------------------------
    # Managed (provisioned) cluster estimates
    # ------------------------------------------------------------------

    def estimate_provisioned_search(
        self,
        size_gb: float,
        azs: int = 3,
        replicas: int = 1,
        target_shard_size_gb: int = 25,
        cpus_per_shard: float = 1.5,
        pricing_type: Literal["OnDemand", "Reserved"] = "OnDemand",
        region: str = DEFAULT_REGION,
    ) -> Dict[str, Any]:
        """Estimate costs for a managed OpenSearch search workload.

        Matches the ``POST /provisioned/estimate`` search workload shape::

            {"search": {"size": 200, "azs": 3, "replicas": 1,
                        "targetShardSize": 25, "CPUsPerShard": 1.5,
                        "pricingType": "OnDemand",
                        "region": "US East (N. Virginia)"}}

        Args:
            size_gb:               Total data size in GB.
            azs:                   Number of Availability Zones.
            replicas:              Number of replicas per primary shard.
            target_shard_size_gb:  Target size per shard in GB (integer).
            cpus_per_shard:        CPU cores allocated per shard.
            pricing_type:          ``"OnDemand"`` or ``"Reserved"``.
            region:                AWS region display name
                                   (e.g. ``"US East (N. Virginia)"``).

        Returns:
            Parsed JSON response from the calculator.
        """
        return self._post(PROVISIONED_ESTIMATE_PATH, {
            "search": {
                "size": size_gb,
                "azs": azs,
                "replicas": replicas,
                "targetShardSize": int(target_shard_size_gb),
                "CPUsPerShard": cpus_per_shard,
                "pricingType": pricing_type,
                "region": region,
            }
        })

    def estimate_provisioned_time_series(
        self,
        size_gb: float,
        azs: int = 3,
        replicas: int = 1,
        hot_retention_days: int = 14,
        warm_retention_days: int = 76,
        target_shard_size_gb: int = 45,
        cpus_per_shard: float = 1.25,
        pricing_type: Literal["OnDemand", "Reserved"] = "OnDemand",
        region: str = DEFAULT_REGION,
    ) -> Dict[str, Any]:
        """Estimate costs for a managed OpenSearch time-series workload.

        Matches the ``POST /provisioned/estimate`` timeSeries workload shape::

            {"timeSeries": {"size": 500, "azs": 3, "replicas": 1,
                            "hotRetentionPeriod": 14, "warmRetentionPeriod": 76,
                            "targetShardSize": 45, "CPUsPerShard": 1.25,
                            "pricingType": "OnDemand",
                            "region": "US East (N. Virginia)"}}

        Args:
            size_gb:               Total data size in GB.
            azs:                   Number of Availability Zones.
            replicas:              Number of replicas per primary shard.
            hot_retention_days:    Days data is kept in hot storage.
            warm_retention_days:   Days data is kept in warm storage.
            target_shard_size_gb:  Target size per shard in GB (integer).
            cpus_per_shard:        CPU cores allocated per shard.
            pricing_type:          ``"OnDemand"`` or ``"Reserved"``.
            region:                AWS region display name.

        Returns:
            Parsed JSON response from the calculator.
        """
        return self._post(PROVISIONED_ESTIMATE_PATH, {
            "timeSeries": {
                "size": size_gb,
                "azs": azs,
                "replicas": replicas,
                "hotRetentionPeriod": hot_retention_days,
                "warmRetentionPeriod": warm_retention_days,
                "targetShardSize": int(target_shard_size_gb),
                "CPUsPerShard": cpus_per_shard,
                "pricingType": pricing_type,
                "region": region,
            }
        })

    def estimate_provisioned_vector(
        self,
        vector_count: int,
        dimensions: int,
        engine_type: Literal[
            "hnswfp32", "hnswfp16", "hnswbq",
            "ivffp32", "ivffp16", "ivfbq"
        ] = "hnswfp16",
        max_edges: int = 16,
        azs: int = 3,
        replicas: int = 1,
        pricing_type: Literal["OnDemand", "Reserved"] = "OnDemand",
        region: str = DEFAULT_REGION,
    ) -> Dict[str, Any]:
        """Estimate costs for a managed OpenSearch vector search workload.

        Args:
            vector_count:  Number of vectors to index.
            dimensions:    Vector dimensionality (e.g. 768 for BERT embeddings).
            engine_type:   HNSW or IVF variant with precision suffix
                           (``fp32``, ``fp16``, ``bq``).
            max_edges:     HNSW ``m`` parameter (max edges per node).
            azs:           Number of Availability Zones.
            replicas:      Number of replicas per primary shard.
            pricing_type:  ``"OnDemand"`` or ``"Reserved"``.
            region:        AWS region display name.

        Returns:
            Parsed JSON response from the calculator.
        """
        return self._post(PROVISIONED_ESTIMATE_PATH, {
            "vector": {
                "vectorCount": vector_count,
                "dimensionsCount": dimensions,
                "vectorEngineType": engine_type,
                "maxEdges": max_edges,
                "azs": azs,
                "replicas": replicas,
                "pricingType": pricing_type,
                "region": region,
            }
        })

    # ------------------------------------------------------------------
    # Serverless estimates
    # ------------------------------------------------------------------

    def estimate_serverless(
        self,
        collection_type: Literal["timeSeries", "search", "vector"],
        daily_index_size: float = 0,
        days_in_hot: int = 1,
        days_in_warm: int = 6,
        collection_size: float = 0,
        min_query_rate: float = 1.0,
        max_query_rate: float = 1.0,
        hours_at_max_rate: float = 0.0,
        region: str = "us-east-1",
        redundancy: bool = True,
    ) -> Dict[str, Any]:
        """Estimate costs for an OpenSearch Serverless collection.

        The API expects different workload fields depending on *collection_type*:

        * ``"timeSeries"`` — uses ``daily_index_size``, ``days_in_hot``,
          ``days_in_warm``, and query-rate parameters.
        * ``"search"`` — uses ``collection_size`` (total collection size in GB)
          and query-rate parameters.  If *collection_size* is not provided it is
          derived from ``daily_index_size * days_in_hot`` for convenience.
        * ``"vector"`` — currently forwards the same fields as *timeSeries*.

        Args:
            collection_type:       ``"timeSeries"``, ``"search"``, or ``"vector"``.
            daily_index_size:      GB of data indexed per day (timeSeries / vector).
            days_in_hot:           Days data is retained in hot storage (timeSeries).
            days_in_warm:          Days data is retained in warm storage (timeSeries).
            collection_size:       Total collection size in GB (search).  When omitted
                                   for a search collection, falls back to
                                   ``daily_index_size * days_in_hot``.
            min_query_rate:        Minimum queries per hour.
            max_query_rate:        Peak queries per hour.
            hours_at_max_rate:     Hours per day running at peak query rate.
            region:                AWS region code (e.g. ``"us-east-1"``).
            redundancy:            Whether to enable multi-AZ redundancy.

        Returns:
            Parsed JSON response from the calculator.
        """
        # The Go API defines minQueryRate / maxQueryRate as int64, so we must
        # send JSON integers — not floats — to avoid an unmarshal error.
        min_qr = int(min_query_rate)
        max_qr = int(max_query_rate)

        if collection_type == "search":
            size = collection_size if collection_size else daily_index_size * days_in_hot
            workload: Dict[str, Any] = {
                "collectionSize": size,
                "minQueryRate": min_qr,
                "maxQueryRate": max_qr,
                "hoursAtMaxRate": hours_at_max_rate,
            }
        else:
            # timeSeries and vector share the same shape
            workload = {
                "dailyIndexSize": daily_index_size,
                "daysInHot": days_in_hot,
                "daysInWarm": days_in_warm,
                "minQueryRate": min_qr,
                "maxQueryRate": max_qr,
                "hoursAtMaxRate": hours_at_max_rate,
            }

        return self._post("/serverless/v2/estimate", {
            collection_type: workload,
            "region": region,
            "redundancy": redundancy,
        })

    # ------------------------------------------------------------------
    # Reference data helpers
    # ------------------------------------------------------------------

    def get_regions(self, deployment: Literal["provisioned", "serverless"] = "provisioned") -> Any:
        """Return available AWS regions for the given deployment type."""
        return self._get(f"/{deployment}/regions")

    def get_pricing_options(self) -> Any:
        """Return available pricing tier options (OnDemand, Reserved, etc.)."""
        return self._get("/provisioned/pricingOptions")

    def get_instance_families(self, region: str) -> Any:
        """Return available instance families for *region*.

        Args:
            region: URL-encoded AWS region display name
                    (e.g. ``"US East (N. Virginia)"``).
        """
        import urllib.parse
        encoded = urllib.parse.quote(region, safe="")
        return self._get(f"/provisioned/instanceFamilyOptions/{encoded}")

    def health_check(self) -> bool:
        """Return True if the calculator service is reachable and healthy."""
        try:
            self._get("/health", timeout=10)
            return True
        except (PricingCalculatorError, OSError):
            return False

    # ------------------------------------------------------------------
    # Convenience: format estimate as a human-readable summary
    # ------------------------------------------------------------------

    @staticmethod
    def _format_provisioned(configs: list) -> list[str]:
        """Format a provisioned (clusterConfigs) estimate into Markdown lines."""
        best = configs[0]
        lines = [f"- **Lowest monthly cost:** ${best['totalCost']:,.2f}"]
        hot = best.get("hotNodes", {})
        if hot.get("type"):
            lines.append(f"- **Hot node type:** {hot['type']} × {hot.get('count', '?')}")
        leader = best.get("leaderNodes", {})
        if leader.get("type"):
            lines.append(f"- **Manager node type:** {leader['type']} × {leader.get('count', '?')}")
        if len(configs) > 1:
            lines.append(
                f"- **Configurations evaluated:** {len(configs)} "
                f"(range ${configs[-1]['totalCost']:,.2f} – ${best['totalCost']:,.2f}/mo)"
            )
        return lines

    @staticmethod
    def _format_serverless(price: dict) -> list[str]:
        """Format a serverless (price) estimate into Markdown lines."""
        lines: list[str] = []
        month = price.get("month", {})
        if "total" in month:
            lines.append(f"- **Monthly cost:** ${month['total']:,.2f}")
        if "indexOcu" in month:
            lines.append(f"  - Index OCU: ${month['indexOcu']:,.2f}")
        if "searchOcu" in month:
            lines.append(f"  - Search OCU: ${month['searchOcu']:,.2f}")
        if "s3Storage" in month:
            lines.append(f"  - S3 storage: ${month['s3Storage']:,.2f}")
        year = price.get("year", {})
        if "total" in year:
            lines.append(f"- **Annual cost:** ${year['total']:,.2f}")
        return lines

    @staticmethod
    def _format_legacy(result: Dict[str, Any]) -> list[str]:
        """Format a legacy flat-shape estimate into Markdown lines."""
        lines: list[str] = []
        if "monthlyCost" in result:
            lines.append(f"- **Monthly cost:** ${result['monthlyCost']:,.2f}")
        if "annualCost" in result:
            lines.append(f"- **Annual cost:** ${result['annualCost']:,.2f}")
        if "instanceType" in result:
            lines.append(f"- **Instance type:** {result['instanceType']}")
        if "instanceCount" in result:
            lines.append(f"- **Instance count:** {result['instanceCount']}")
        if "storageGB" in result:
            lines.append(f"- **Storage:** {result['storageGB']:,} GB")
        if "shardCount" in result:
            lines.append(f"- **Shards:** {result['shardCount']}")
        return lines

    @staticmethod
    def format_estimate(result: Dict[str, Any]) -> str:
        """Return a compact Markdown summary of an estimate response.

        Handles both provisioned (clusterConfigs) and serverless (price.month)
        response shapes. Falls back to pretty-printed JSON for unknown shapes.
        """
        try:
            configs = result.get("clusterConfigs")
            if configs and isinstance(configs, list):
                return "\n".join(PricingCalculatorClient._format_provisioned(configs))

            price = result.get("price")
            if price and isinstance(price, dict):
                lines = PricingCalculatorClient._format_serverless(price)
                if lines:
                    return "\n".join(lines)

            lines = PricingCalculatorClient._format_legacy(result)
            if lines:
                return "\n".join(lines)
        except (TypeError, KeyError):
            pass
        return f"```json\n{json.dumps(result, indent=2)}\n```"
