---
title: Source configuration
sidebar_position: 5
---

Quickwit can insert data into an index from one or multiple sources.
A source can be added after index creation using the [CLI command](../reference/cli.md#source) `quickwit source create`.
It can also be enabled or disabled with the `quickwit source enable/disable` subcommands.

A source is declared using an object called source config, which defines the source's settings. It consists of multiple parameters:

- source ID
- source type
- source parameters
- input_format
- maximum number of pipelines per indexer (optional)
- desired number of pipelines (optional)
- transform parameters (optional)

## Source ID

The source ID is a string that uniquely identifies the source within an index. It may only contain uppercase or lowercase ASCII letters, digits, hyphens (`-`), and underscores (`_`). Finally, it must start with a letter and contain at least 3 characters but no more than 255.

## Source type

The source type designates the kind of source being configured. As of version 0.5, available source types are `ingest-api`, `kafka`, `kinesis`, and `pulsar`. The `file` type is also supported but only for local ingestion from [the CLI](/docs/reference/cli.md#tool-local-ingest).

## Source parameters

The source parameters indicate how to connect to a data store and are specific to the source type.

### File source (CLI only)

A file source reads data from a local file. The file must consist of JSON objects separated by a newline (NDJSON).
As of version 0.5, a file source can only be ingested with the [CLI command](/docs/reference/cli.md#tool-local-ingest). Compressed files (bz2, gzip, ...) and remote files (Amazon S3, HTTP, ...) are not supported.

```bash
./quickwit tool local-ingest --input-path <INPUT_PATH>
```

### Ingest API source

An ingest API source reads data from the [Ingest API](/docs/reference/rest-api.md#ingest-data-into-an-index). This source is automatically created at the index creation and cannot be deleted nor disabled.

### Kafka source

A Kafka source reads data from a Kafka stream. Each message in the stream must hold a JSON object.

A tutorial is available [here](/docs/ingest-data/kafka.md).

#### Kafka source parameters

The Kafka source consumes a `topic` using the client library [librdkafka](https://github.com/edenhill/librdkafka) and forwards the key-value pairs carried by the parameter `client_params` to the underlying librdkafka consumer. Common `client_params` options are bootstrap servers (`bootstrap.servers`), or security protocol (`security.protocol`). Please, refer to [Kafka](https://kafka.apache.org/documentation/#consumerconfigs) and [librdkafka](https://github.com/edenhill/librdkafka/blob/master/CONFIGURATION.md) documentation pages for more advanced options.

| Property | Description | Default value |
| --- | --- | --- |
| `topic` | Name of the topic to consume. | required |
| `client_log_level` | librdkafka client log level. Possible values are: debug, info, warn, error. | `info` |
| `client_params` | librdkafka client configuration parameters. | `{}` |
| `enable_backfill_mode` | Backfill mode stops the source after reaching the end of the topic. | `false` |

**Kafka client parameters**

- `bootstrap.servers`
Comma-separated list of host and port pairs that are the addresses of a subset of the Kafka brokers in the Kafka cluster.

- `auto.offset.reset`
Defines the behavior of the source when consuming a partition for which there is no initial offset saved in the checkpoint. `earliest` consumes from the beginning of the partition, whereas `latest` (default) consumes from the end.

- `enable.auto.commit`
The Kafka source manages commit offsets manually using the [checkpoint API](../overview/concepts/indexing.md#checkpoint) and disables auto-commit.

- `group.id`
Kafka-based distributed indexing relies on consumer groups. Unless overridden in the client parameters, the default group ID assigned to each consumer managed by the source is `quickwit-{index_uid}-{source_id}`.

- `max.poll.interval.ms`
Short max poll interval durations may cause a source to crash when back pressure from the indexer occurs. Therefore, Quickwit recommends using the default value of `300000` (5 minutes).

*Adding a Kafka source to an index with the [CLI](../reference/cli.md#source)*

```bash
cat << EOF > source-config.yaml
version: 0.6
source_id: my-kafka-source
source_type: kafka
max_num_pipelines_per_indexer: 1
desired_num_pipelines: 2
params:
  topic: my-topic
  client_params:
    bootstrap.servers: localhost:9092
    security.protocol: SSL
EOF
./quickwit source create --index my-index --source-config source-config.yaml
```

### Kinesis source

A Kinesis source reads data from an [Amazon Kinesis](https://aws.amazon.com/kinesis/) stream. Each message in the stream must hold a JSON object.

A tutorial is available [here](/docs/ingest-data/kinesis.md).

**Kinesis source parameters**

The Kinesis source consumes a stream identified by a `stream_name` and a `region`.

| Property | Description | Default value |
| --- | --- | --- |
| `stream_name` | Name of the stream to consume. | required |
| `region` | The AWS region of the stream. Mutually exclusive with `endpoint`. | `us-east-1` |
| `endpoint` | Custom endpoint for use with AWS-compatible Kinesis service. Mutually exclusive with `region`. | optional |

If no region is specified, Quickwit will attempt to find one in multiple other locations and with the following order of precedence:

1. Environment variables (`AWS_REGION` then `AWS_DEFAULT_REGION`)

2. Config file, typically located at `~/.aws/config` or otherwise specified by the `AWS_CONFIG_FILE` environment variable if set and not empty.

3. Amazon EC2 instance metadata service determining the region of the currently running Amazon EC2 instance.

4. Default value: `us-east-1`

*Adding a Kinesis source to an index with the [CLI](../reference/cli.md#source)*

```bash
cat << EOF > source-config.yaml
version: 0.6
source_id: my-kinesis-source
source_type: kinesis
params:
  stream_name: my-stream
EOF
quickwit source create --index my-index --source-config source-config.yaml
```

### Pulsar source

A Puslar source reads data from one or several Pulsar topics. Each message in topic(s) must hold a JSON object.

A tutorial is available [here](/docs/ingest-data/pulsar.md).

**Pulsar source parameters**

The Pulsar source consumes `topics` using the client library [pulsar-rs](https://github.com/streamnative/pulsar-rs).

| Property | Description | Default value |
| --- | --- | --- |
| `topics` | List of topics to consume. | required |
| `address` | Pulsar URL (pulsar:// and pulsar+ssl://). | required |
| `consumer_name` | The consumer name to register with the pulsar source. | `quickwit` |

*Adding a Pulsar source to an index with the [CLI](../reference/cli.md#source)*

```bash
cat << EOF > source-config.yaml
version: 0.6
source_id: my-pulsar-source
source_type: pulsar
params:
  topics:
    - my-topic
  address: pulsar://localhost:6650
EOF
./quickwit source create --index my-index --source-config source-config.yaml
```

## Maximum number of pipelines per indexer

The `max_num_pipelines_per_indexer` parameter is only available for sources that can be distributed: Kafka, GCP PubSub and Pulsar(coming soon).

The maximum number of indexing pipelines defines the limit of pipelines spawned for the source on a given indexer.
This maximum can be reached only if there are enough `desired_num_pipelines` to run.

:::note

With the following parameters, only one pipeline will run on one indexer.

- `max_num_pipelines_per_indexer=2`
- `desired_num_pipelines=1`

:::

## Desired number of pipelines

`desired_num_pipelines` parameter is only available for sources that can be distributed: Kafka, GCP PubSub and Pulsar (coming soon).

The desired number of indexing pipelines defines the number of pipelines to run on a cluster for the source. It is a "desired"
number as it cannot be reach it there is not enough indexers in
the cluster.

:::note

With the following parameters, only one pipeline will start on the sole indexer.

- `max_num_pipelines_per_indexer=1`
- `desired_num_pipelines=2`

:::

## Transform parameters

For all source types but the `ingest-api`, ingested documents can be transformed before being indexed using [Vector Remap Language (VRL)](https://vector.dev/docs/reference/vrl/) scripts.

| Property | Description | Default value |
| --- | --- | --- |
| `script` | Source code of the VRL program executed to transform documents. | required |
| `timezone` | Timezone used in the VRL program for date and time manipulations. It must be a valid name in the [TZ database](https://en.wikipedia.org/wiki/List_of_tz_database_time_zones) | `UTC` |

```yaml
# Your source config here
# ...
transform:
  script: |
    .message = downcase(string!(.message))
    .timestamp = now()
    del(.username)
  timezone: local
```

## Input format

The `input_format` parameter specifies the expected data format of the source. Two formats are currently supported:
- `json`: JSON, the default
- `plain_text`: unstructured text document

Internally, Quickwit can only index JSON data. To allow the ingestion of plain text documents, Quickwit transform them on the fly into JSON objects of the following form: `{"plain_text": "<original plain text document>"}`. Then, they can be optionally transformed into more complex documents using a VRL script. (see [transform feature](#transform-parameters)).

The following is an example of how one could parse and transform a CSV dataset containing a list of users described by 3 attributes: first name, last name, and age.

```yaml
# Your source config here
# ...
transform:
  script: |
    user = parse_csv!(.plain_text)
    .first_name = user[0]
    .last_name = user[1]
    .age = to_int!(user[2])
    del(.plain_text)
```

## Enabling/Disabling a source from an index

A source can be enabled or disabled from an index using the [CLI command](../reference/cli.md) `quickwit source enable` or `quickwit source disable`:

```bash
quickwit source disable --index my-index --source my-source
```

A source is enabled by default. When disabling a source, the related indexing pipelines will be shut down on each relevant indexer and indexing for this source will be paused.

## Deleting a source from an index

A source can be removed from an index using the [CLI command](../reference/cli.md) `quickwit source delete`:

```bash
quickwit source delete --index my-index --source my-source
```

When deleting a source, the checkpoint associated with the source is also removed.
