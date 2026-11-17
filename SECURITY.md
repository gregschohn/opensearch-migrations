## Reporting a Vulnerability

If you discover a potential security issue in this project we ask that you notify OpenSearch Security directly via email to security@opensearch.org. Please do **not** create a public GitHub issue.

## While using the Traffic Capture & Replay tools

We strive to make these tools reliable and secure to use. However, before you begin using them with production or potentially sensitive data, we'd like to highlight a few points you should be aware of.

### Requests replayed with more permissive credentials

When configuring the Traffic Replayer, you can provide a set of credentials that will be attached to the replayed requests. It could be possible for an attacker to send a series of requests that they are not authorized to execute against the source cluster. The requests would be denied on the source cluster, but replayed against the target cluster with the configured credentials which may have higher privileges and allow the requests to be executed. The attacker could not directly read data this way, but any side effects of the requests (e.g. deleting data) would be executed.

### Denial of Service against the source cluster

In this case, an attacker could send a series of large and potentially malformed requests to the source cluster via the capture proxy. These messages would be relayed to the Kafka cluster, and if they were PUT/POST/UPDATE requests, block sending the request to the source cluster until the message was finished. If the attacker is able to use this strategy to tie up the proxy and/or Kafka cluster, all other incoming mutating requests to the source cluster would be blocked.

We have partially mitigated this by bounding the proxy's Kafka publication in both time and memory (see the [Capture Proxy README](./TrafficCapture/captureProxy/README.md)). A record that Kafka has not acknowledged within the producer's delivery timeout (10 seconds by default, `delivery.timeout.ms` in the file passed with `--kafka-property-file`) is a capture failure, handled according to `--capture-failure-policy`. Records awaiting acknowledgement are limited by a publication budget (64 MiB and 4,096 records by default, set with `--kafka-publication-budget-bytes` and `--kafka-publication-budget-records`); when it is full, the proxy stops reading from clients until acknowledgements free space instead of buffering more. The flow of messages to Kafka, and with it every captured connection, could still be disrupted.

If you are concerned about this scenario, we recommend fully mitigating it by putting a load-shedder in front of the proxy.

### Credentials are available in the output tuples

The output tuples, available on the shared EFS volume via the Migration Console, contain the exact requests and responses received from both the source and target clusters with the headers and the body of the messages. The Authorization header is present on SigV4 signed requests and those using basic authorization, and with basic authorization credentials can be extracted from the header value. These values are often essential for debugging and so are not censored from the output.

If you use basic authorization credentials, ensure that access to your output tuples is protected similarly to the credentials themselves.

### Customer Managed Keys are not supported by the migration infrastructure
Each of the AWS services that are interacting with data will encrypt all data being stored at rest. While the services themselves can support performing the encryption via a KMS Key, the CDK deployment option of Migration Assistant doesn't have the ability to set a customer key for any of those services. That will leave all of the data at rest encrypted, but not under the control of a customer's KMS Key. See the links below for more details on forthcoming support:

https://docs.aws.amazon.com/kms/latest/developerguide/concepts.html
[Issue #1026](https://github.com/opensearch-project/opensearch-migrations/issues/1026)
