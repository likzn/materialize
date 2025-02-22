# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set schema={
    "name": "row",
    "type": "record",
    "fields": [
      {"name": "a", "type": "long"}
    ]
  }

$ kafka-create-topic topic=data

$ kafka-ingest format=avro topic=data schema=${schema} publish=true timestamp=1
{"a": 1}

> CREATE SECRET ssl_key_kafka AS '${arg.materialized-kafka-key}'
> CREATE SECRET ssl_key_csr AS '${arg.materialized-schema-registry-key}'
> CREATE SECRET password_csr AS 'sekurity'

# Ensure connections track their dependencies
> CREATE CONNECTION kafka_ssl
  FOR KAFKA
    BROKER 'kafka:9092',
    SSL KEY = SECRET ssl_key_kafka,
    SSL CERTIFICATE = '${arg.materialized-kafka-crt}',
    SSL CERTIFICATE AUTHORITY = '${arg.ca-crt}';

! DROP SECRET ssl_key_kafka;
contains: cannot drop

# Ensure non-secrets cannot be used as secrets
> CREATE TABLE not_a_secret (a int);

! CREATE MATERIALIZED SOURCE data
  FROM KAFKA BROKER 'kafka:9092' TOPIC 'testdrive-data-${testdrive.seed}'
  WITH (
      ssl_key_pem = SECRET not_a_secret
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
contains: materialize.public.not_a_secret is not a secret

> CREATE MATERIALIZED SOURCE data
  FROM KAFKA BROKER 'kafka:9092' TOPIC 'testdrive-data-${testdrive.seed}'
  WITH (
      security_protocol = 'SSL',
      ssl_key_pem = SECRET ssl_key_kafka,
      ssl_certificate_pem = '${arg.materialized-kafka-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH (
      ssl_key_pem = SECRET ssl_key_csr,
      ssl_certificate_pem = '${arg.materialized-schema-registry-crt}',
      ssl_ca_pem = '${arg.ca-crt}',
      username = "materialize",
      password = SECRET password_csr
  )

> SELECT * FROM data
a
---
1

$ kafka-ingest format=avro topic=data schema=${schema} timestamp=2
{"a": 2}

> SELECT * FROM data
a
---
1
2

> CREATE SINK snk FROM data
  INTO KAFKA BROKER 'kafka' TOPIC 'snk'
  WITH (
      security_protocol = 'SSL',
      ssl_key_pem = SECRET ssl_key_kafka,
      ssl_certificate_pem = '${arg.materialized-kafka-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH (
      ssl_key_pem = SECRET ssl_key_csr,
      ssl_certificate_pem = '${arg.materialized-schema-registry-crt}',
      ssl_ca_pem = '${arg.ca-crt}',
      username = "materialize",
      password = SECRET password_csr
  )

$ kafka-verify format=avro sink=materialize.public.snk sort-messages=true
{"before": null, "after": {"row":{"a": 1}}}
{"before": null, "after": {"row":{"a": 2}}}

# Check environment variables; defined in mzcompose.py

> CREATE SINK env_pw_snk FROM data
  INTO KAFKA BROKER 'kafka' TOPIC 'snk'
  WITH (
      security_protocol = 'SSL',
      ssl_key_pem = SECRET ssl_key_kafka,
      ssl_certificate_pem = '${arg.materialized-kafka-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH (
      ssl_key_pem = SECRET ssl_key_csr,
      ssl_certificate_pem = '${arg.materialized-schema-registry-crt}',
      ssl_ca_pem = '${arg.ca-crt}',
      username = "materialize",
      password = SECRET password_csr
  )
  ENVELOPE DEBEZIUM

$ kafka-verify format=avro sink=materialize.public.env_pw_snk sort-messages=true
{"before": null, "after": {"row":{"a": 1}}}
{"before": null, "after": {"row":{"a": 2}}}

# not basic_auth
! CREATE SINK no_basic_auth FROM data
  INTO KAFKA BROKER 'kafka' TOPIC 'snk'
  WITH (
      security_protocol = 'SSL',
      ssl_key_pem = SECRET ssl_key_kafka,
      ssl_certificate_pem = '${arg.materialized-kafka-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH (
      ssl_key_pem = SECRET ssl_key_csr,
      ssl_certificate_pem = '${arg.materialized-schema-registry-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
contains:error publishing kafka schemas for sink: unable to publish value schema to registry in kafka sink: server error 401: Unauthorized

# Ensure that we get an ssl error if we forget to set certs
! CREATE MATERIALIZED SOURCE data
  FROM KAFKA BROKER 'kafka:9092' TOPIC 'testdrive-data-${testdrive.seed}'
  WITH (
      security_protocol = 'SSL',
      ssl_key_pem = SECRET ssl_key_kafka,
      ssl_certificate_pem = '${arg.materialized-kafka-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
contains:self signed certificate in certificate chain

# missing config
! CREATE MATERIALIZED SOURCE data
  FROM KAFKA BROKER 'kafka:9092' TOPIC 'testdrive-data-${testdrive.seed}'
  WITH (
      security_protocol = 'SSL',
      ssl_certificate_pem = '${arg.materialized-kafka-crt}',
      ssl_ca_pem = '${arg.ca-crt}'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
contains:missing SSL KEY (ssl.key.pem)

> CREATE MATERIALIZED SOURCE kafka_connector_source
  FROM KAFKA CONNECTION kafka_ssl
  TOPIC 'testdrive-data-${testdrive.seed}'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH (
      ssl_key_pem = SECRET ssl_key_csr,
      ssl_certificate_pem = '${arg.materialized-schema-registry-crt}',
      ssl_ca_pem = '${arg.ca-crt}',
      username = "materialize",
      password = SECRET password_csr
  )

> SELECT * FROM kafka_connector_source
a
---
1
2

> CREATE CONNECTION csr_ssl
  FOR CONFLUENT SCHEMA REGISTRY
    URL '${testdrive.schema-registry-url}',
    SSL KEY = SECRET ssl_key_csr,
    SSL CERTIFICATE = '${arg.materialized-schema-registry-crt}',
    SSL CERTIFICATE AUTHORITY = '${arg.ca-crt}',
    USERNAME = 'materialize',
    PASSWORD = SECRET password_csr;

> CREATE MATERIALIZED SOURCE kafka_csr_connector_source
  FROM KAFKA CONNECTION kafka_ssl
    TOPIC 'testdrive-data-${testdrive.seed}'
    FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_ssl


> SELECT * FROM kafka_csr_connector_source
a
---
1
2

# Options that conflict between CONNECTION and WITH error

! CREATE SOURCE IF NOT EXISTS duplicate_option_specified
  FROM KAFKA CONNECTION kafka_ssl
  TOPIC 'testdrive-data-${testdrive.seed}'
  WITH (
    security_protocol = 'ssl'
  )
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'
  WITH (
      ssl_key_pem = SECRET ssl_key_csr,
      ssl_certificate_pem = '${arg.materialized-schema-registry-crt}',
      ssl_ca_pem = '${arg.ca-crt}',
      username = "materialize",
      password = SECRET password_csr
  )
contains:cannot set option security.protocol for SOURCE using CONNECTION materialize.public.kafka_ssl

# Ensure that connectors do not require the certificate authority

> CREATE CONNECTION kafka_sasl_no_ca
  FOR KAFKA
    BROKER 'kafka:9092',
    SSL KEY = SECRET ssl_key_kafka,
    SSL CERTIFICATE = '${arg.materialized-kafka-crt}';

>  CREATE CONNECTION csr_ssl_no_ca
  FOR CONFLUENT SCHEMA REGISTRY
    URL '${testdrive.schema-registry-url}',
    SSL KEY = SECRET ssl_key_csr,
    SSL CERTIFICATE = '${arg.materialized-schema-registry-crt}',
    USERNAME = 'materialize',
    PASSWORD = SECRET password_csr;

# This ensures that the error is not that the CA was required, but simply that
# not providing it prohibits connecting.
! CREATE SOURCE kafka_ssl_no_ca
  FROM KAFKA CONNECTION kafka_sasl_no_ca
    TOPIC 'testdrive-data-${testdrive.seed}'
    FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_ssl_no_ca
contains: broker certificate could not be verified
