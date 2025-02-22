// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! SQL purification.
//!
//! See the [crate-level documentation](crate) for details.

use std::error::Error as StdError;
use std::iter;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use aws_arn::ResourceName as AmazonResourceName;
use mz_kafka_util::KafkaAddrs;
use mz_sql_parser::ast::{
    CsrConnection, CsrSeedAvro, CsrSeedProtobuf, CsrSeedProtobufSchema, KafkaConnection,
    KafkaSourceConnection, ReaderSchemaSelectionStrategy,
};
use prost::Message;
use protobuf_native::compiler::{SourceTreeDescriptorDatabase, VirtualSourceTree};
use protobuf_native::MessageLite;
use tracing::info;
use uuid::Uuid;

use mz_ccsr::Schema as CcsrSchema;
use mz_ccsr::{Client, GetByIdError, GetBySubjectError};
use mz_proto::RustType;
use mz_repr::strconv;
use mz_storage::types::connections::aws::{AwsConfig, AwsExternalIdPrefix};
use mz_storage::types::connections::{Connection, ConnectionContext};
use mz_storage::types::sources::PostgresSourceDetails;

use crate::ast::{
    AvroSchema, CreateSourceConnection, CreateSourceFormat, CreateSourceStatement,
    CsrConnectionAvro, CsrConnectionProtobuf, CsvColumns, DbzMode, Envelope, Format, Ident,
    ProtobufSchema, Value, WithOption, WithOptionValue,
};
use crate::catalog::SessionCatalog;
use crate::kafka_util;
use crate::names::Aug;
use crate::normalize;
use crate::plan::StatementContext;

/// Purifies a statement, removing any dependencies on external state.
///
/// See the section on [purification](crate#purification) in the crate
/// documentation for details.
///
/// Note that purification is asynchronous, and may take an unboundedly long
/// time to complete. As a result purification does *not* have access to a
/// [`SessionCatalog`](crate::catalog::SessionCatalog), as that would require
/// locking access to the catalog for an unbounded amount of time.
pub async fn purify_create_source(
    catalog: Box<dyn SessionCatalog>,
    now: u64,
    mut stmt: CreateSourceStatement<Aug>,
    connection_context: ConnectionContext,
) -> Result<CreateSourceStatement<Aug>, anyhow::Error> {
    let CreateSourceStatement {
        connection,
        format,
        envelope,
        with_options,
        include_metadata: _,
        ..
    } = &mut stmt;

    let _ = catalog;

    let mut with_options_map = normalize::options(with_options)?;

    match connection {
        CreateSourceConnection::Kafka(KafkaSourceConnection {
            connection, topic, ..
        }) => {
            // Extract any/all configuration options
            let mut connection_options = kafka_util::extract_config(&mut with_options_map)?;

            let connection = match connection {
                KafkaConnection::Reference { connection } => {
                    let scx = StatementContext::new(None, &*catalog);
                    let item = scx.get_item_by_resolved_name(&connection)?;
                    // Get Kafka connection
                    match item.connection()? {
                        Connection::Kafka(connection) => connection.clone(),
                        _ => bail!("{} is not a kafka connection", item.name()),
                    }
                }
                KafkaConnection::Inline { broker } => {
                    // Add broker option
                    connection_options.insert(
                        "bootstrap.servers".into(),
                        KafkaAddrs::from_str(&broker)?.to_string().into(),
                    );

                    mz_storage::types::connections::KafkaConnection::try_from(
                        &mut connection_options,
                    )?
                }
            };
            let consumer = kafka_util::create_consumer(
                &topic,
                &connection,
                &connection_options,
                connection_context.librdkafka_log_level,
                &*connection_context.secrets_reader,
            )
            .await
            .map_err(|e| anyhow!("Failed to create and connect Kafka consumer: {}", e))?;

            // Translate `kafka_time_offset` to `start_offset`.
            match kafka_util::lookup_start_offsets(
                Arc::clone(&consumer),
                &topic,
                &with_options_map,
                now,
            )
            .await?
            {
                Some(start_offsets) => {
                    // Drop `kafka_time_offset`
                    with_options.retain(|val| match val {
                        WithOption { key, .. } => key.as_str() != "kafka_time_offset",
                    });
                    info!("add start_offset {:?}", start_offsets);
                    // Add `start_offset`
                    with_options.push(WithOption {
                        key: Ident::new("start_offset"),
                        value: Some(WithOptionValue::Value(Value::Array(
                            start_offsets
                                .iter()
                                .map(|offset| Value::Number(offset.to_string()))
                                .collect(),
                        ))),
                    });
                }
                None => {}
            }
        }
        CreateSourceConnection::S3 { .. } => {
            let aws_config = normalize::aws_config(&mut with_options_map, None)?;
            validate_aws_credentials(
                &aws_config,
                connection_context.aws_external_id_prefix.as_ref(),
            )
            .await?;
        }
        CreateSourceConnection::Kinesis { arn } => {
            let region = arn
                .parse::<AmazonResourceName>()
                .context("Unable to parse provided ARN")?
                .region
                .ok_or_else(|| anyhow!("Provided ARN does not include an AWS region"))?;

            let aws_config = normalize::aws_config(&mut with_options_map, Some(region.into()))?;
            validate_aws_credentials(
                &aws_config,
                connection_context.aws_external_id_prefix.as_ref(),
            )
            .await?;
        }
        CreateSourceConnection::Postgres {
            connection,
            publication,
            details: details_ast,
        } => {
            let scx = StatementContext::new(None, &*catalog);
            let connection = {
                let item = scx.get_item_by_resolved_name(&connection)?;
                match item.connection()? {
                    Connection::Postgres(connection) => connection.clone(),
                    _ => bail!("{} is not a postgres connection", item.name()),
                }
            };
            // verify that we can connect upstream and snapshot publication metadata
            let config = connection
                .config(&*connection_context.secrets_reader)
                .await?;
            let tables = mz_postgres_util::publication_info(&config, &publication).await?;

            let details = PostgresSourceDetails {
                tables,
                slot: format!(
                    "materialize_{}",
                    Uuid::new_v4().to_string().replace('-', "")
                ),
            };
            *details_ast = Some(hex::encode(details.into_proto().encode_to_vec()));
        }
        CreateSourceConnection::PubNub { .. } => (),
    }

    purify_source_format(
        &*catalog,
        format,
        connection,
        &envelope,
        &connection_context,
    )
    .await?;

    Ok(stmt)
}

async fn purify_source_format(
    catalog: &dyn SessionCatalog,
    format: &mut CreateSourceFormat<Aug>,
    connection: &mut CreateSourceConnection<Aug>,
    envelope: &Option<Envelope<Aug>>,
    connection_context: &ConnectionContext,
) -> Result<(), anyhow::Error> {
    if matches!(format, CreateSourceFormat::KeyValue { .. })
        && !matches!(connection, CreateSourceConnection::Kafka { .. })
    {
        bail!("Kafka sources are the only source type that can provide KEY/VALUE formats")
    }

    match format {
        CreateSourceFormat::None => {}
        CreateSourceFormat::Bare(format) => {
            purify_source_format_single(catalog, format, connection, envelope, connection_context)
                .await?;
        }

        CreateSourceFormat::KeyValue { key, value: val } => {
            purify_source_format_single(catalog, key, connection, envelope, connection_context)
                .await?;
            purify_source_format_single(catalog, val, connection, envelope, connection_context)
                .await?;
        }
    }
    Ok(())
}

async fn purify_source_format_single(
    catalog: &dyn SessionCatalog,
    format: &mut Format<Aug>,
    connection: &mut CreateSourceConnection<Aug>,
    envelope: &Option<Envelope<Aug>>,
    connection_context: &ConnectionContext,
) -> Result<(), anyhow::Error> {
    match format {
        Format::Avro(schema) => match schema {
            AvroSchema::Csr { csr_connection } => {
                purify_csr_connection_avro(
                    catalog,
                    connection,
                    csr_connection,
                    envelope,
                    connection_context,
                )
                .await?
            }
            AvroSchema::InlineSchema { schema, .. } => {
                if let mz_sql_parser::ast::Schema::File(path) = schema {
                    let file_schema = tokio::fs::read_to_string(path).await?;
                    *schema = mz_sql_parser::ast::Schema::Inline(file_schema);
                }
            }
        },
        Format::Protobuf(schema) => match schema {
            ProtobufSchema::Csr { csr_connection } => {
                purify_csr_connection_proto(
                    catalog,
                    connection,
                    csr_connection,
                    envelope,
                    connection_context,
                )
                .await?;
            }
            ProtobufSchema::InlineSchema {
                message_name: _,
                schema,
            } => {
                if let mz_sql_parser::ast::Schema::File(path) = schema {
                    let descriptors = tokio::fs::read(path).await?;
                    let mut buf = String::new();
                    strconv::format_bytes(&mut buf, &descriptors);
                    *schema = mz_sql_parser::ast::Schema::Inline(buf);
                }
            }
        },
        Format::Csv {
            delimiter: _,
            ref mut columns,
        } => {
            if let CsvColumns::Header { names } = columns {
                match connection {
                    CreateSourceConnection::S3 { .. } => {
                        if names.is_empty() {
                            bail!("CSV WITH HEADER for S3 sources requires specifying the header columns");
                        }
                    }
                    _ => bail!("CSV WITH HEADER is only supported for S3 sources"),
                }
            }
        }
        Format::Bytes | Format::Regex(_) | Format::Json | Format::Text => (),
    }
    Ok(())
}

async fn purify_csr_connection_proto(
    catalog: &dyn SessionCatalog,
    connection: &mut CreateSourceConnection<Aug>,
    csr_connection: &mut CsrConnectionProtobuf<Aug>,
    envelope: &Option<Envelope<Aug>>,
    connection_context: &ConnectionContext,
) -> Result<(), anyhow::Error> {
    let topic =
        if let CreateSourceConnection::Kafka(KafkaSourceConnection { topic, .. }) = connection {
            topic
        } else {
            bail!("Confluent Schema Registry is only supported with Kafka sources")
        };

    let CsrConnectionProtobuf {
        connection,
        seed,
        with_options: ccsr_options,
    } = csr_connection;
    match seed {
        None => {
            let ccsr_connection = match connection {
                CsrConnection::Inline { url } => kafka_util::generate_ccsr_connection(
                    url.parse()?,
                    &mut normalize::options(&ccsr_options)?,
                )?,
                CsrConnection::Reference { connection } => {
                    let scx = StatementContext::new(None, &*catalog);
                    let item = scx.get_item_by_resolved_name(&connection)?;
                    match item.connection()? {
                        Connection::Csr(connection) => connection.clone(),
                        _ => bail!("{} is not a schema registry connection", item.name()),
                    }
                }
            };

            let ccsr_client = ccsr_connection
                .connect(&*connection_context.secrets_reader)
                .await?;

            let value = compile_proto(&format!("{}-value", topic), &ccsr_client).await?;
            let key = compile_proto(&format!("{}-key", topic), &ccsr_client)
                .await
                .ok();

            if matches!(envelope, Some(Envelope::Debezium(DbzMode::Upsert))) && key.is_none() {
                bail!("Key schema is required for ENVELOPE DEBEZIUM UPSERT");
            }

            *seed = Some(CsrSeedProtobuf { value, key });
        }
        Some(_) => (),
    }

    Ok(())
}

async fn purify_csr_connection_avro(
    catalog: &dyn SessionCatalog,
    connection: &mut CreateSourceConnection<Aug>,
    csr_connection: &mut CsrConnectionAvro<Aug>,
    envelope: &Option<Envelope<Aug>>,
    connection_context: &ConnectionContext,
) -> Result<(), anyhow::Error> {
    let topic =
        if let CreateSourceConnection::Kafka(KafkaSourceConnection { topic, .. }) = connection {
            topic
        } else {
            bail!("Confluent Schema Registry is only supported with Kafka sources")
        };

    let CsrConnectionAvro {
        connection,
        seed,
        key_strategy,
        value_strategy,
        with_options: ccsr_options,
    } = csr_connection;
    if seed.is_none() {
        let ccsr_connection = match connection {
            CsrConnection::Inline { url } => kafka_util::generate_ccsr_connection(
                url.parse()?,
                &mut normalize::options(&ccsr_options)?,
            )?,
            CsrConnection::Reference { connection } => {
                let scx = StatementContext::new(None, &*catalog);
                let item = scx.get_item_by_resolved_name(&connection)?;
                match item.connection()? {
                    Connection::Csr(connection) => connection.clone(),
                    _ => bail!("{} is not a schema registry connection", item.name()),
                }
            }
        };
        let ccsr_client = ccsr_connection
            .connect(&*connection_context.secrets_reader)
            .await?;
        let Schema {
            key_schema,
            value_schema,
        } = get_remote_csr_schema(
            &ccsr_client,
            key_strategy.clone().unwrap_or_default(),
            value_strategy.clone().unwrap_or_default(),
            topic.clone(),
        )
        .await?;
        if matches!(envelope, Some(Envelope::Debezium(DbzMode::Upsert))) && key_schema.is_none() {
            bail!("Key schema is required for ENVELOPE DEBEZIUM UPSERT");
        }

        *seed = Some(CsrSeedAvro {
            key_schema,
            value_schema,
        })
    }

    Ok(())
}

#[derive(Debug)]
pub struct Schema {
    pub key_schema: Option<String>,
    pub value_schema: String,
}

#[derive(Debug)]
pub enum GetSchemaError {
    Subject(GetBySubjectError),
    Id(GetByIdError),
}

impl From<GetBySubjectError> for GetSchemaError {
    fn from(inner: GetBySubjectError) -> Self {
        Self::Subject(inner)
    }
}

impl From<GetByIdError> for GetSchemaError {
    fn from(inner: GetByIdError) -> Self {
        Self::Id(inner)
    }
}

impl std::fmt::Display for GetSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetSchemaError::Subject(e) => write!(f, "failed to look up schema by subject: {e}"),
            GetSchemaError::Id(e) => write!(f, "failed to look up schema by id: {e}"),
        }
    }
}

impl StdError for GetSchemaError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            GetSchemaError::Subject(e) => Some(e),
            GetSchemaError::Id(e) => Some(e),
        }
    }
}

async fn get_schema_with_strategy(
    client: &Client,
    strategy: ReaderSchemaSelectionStrategy,
    subject: &str,
) -> Result<Option<String>, GetSchemaError> {
    match strategy {
        ReaderSchemaSelectionStrategy::Latest => {
            match client.get_schema_by_subject(subject).await {
                Ok(CcsrSchema { raw, .. }) => Ok(Some(raw)),
                Err(GetBySubjectError::SubjectNotFound) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        ReaderSchemaSelectionStrategy::Inline(raw) => Ok(Some(raw)),
        ReaderSchemaSelectionStrategy::ById(id) => match client.get_schema_by_id(id).await {
            Ok(CcsrSchema { raw, .. }) => Ok(Some(raw)),
            Err(GetByIdError::SchemaNotFound) => Ok(None),
            Err(e) => Err(e.into()),
        },
    }
}

async fn get_remote_csr_schema(
    ccsr_client: &mz_ccsr::Client,
    key_strategy: ReaderSchemaSelectionStrategy,
    value_strategy: ReaderSchemaSelectionStrategy,
    topic: String,
) -> Result<Schema, anyhow::Error> {
    let value_schema_name = format!("{}-value", topic);
    let value_schema = get_schema_with_strategy(&ccsr_client, value_strategy, &value_schema_name)
        .await
        .with_context(|| {
            format!(
                "fetching latest schema for subject '{}' from registry",
                value_schema_name
            )
        })?;
    let value_schema = value_schema.ok_or_else(|| anyhow!("No value schema found"))?;
    let subject = format!("{}-key", topic);
    let key_schema = get_schema_with_strategy(&ccsr_client, key_strategy, &subject).await?;
    Ok(Schema {
        key_schema,
        value_schema,
    })
}

/// Collect protobuf message descriptor from CSR and compile the descriptor.
async fn compile_proto(
    subject_name: &String,
    ccsr_client: &Client,
) -> Result<CsrSeedProtobufSchema, anyhow::Error> {
    let (primary_subject, dependency_subjects) =
        ccsr_client.get_subject_and_references(subject_name).await?;

    // Compile .proto files into a file descriptor set.
    let mut source_tree = VirtualSourceTree::new();
    for subject in iter::once(&primary_subject).chain(dependency_subjects.iter()) {
        source_tree.as_mut().add_file(
            Path::new(&subject.name),
            subject.schema.raw.as_bytes().to_vec(),
        );
    }
    let mut db = SourceTreeDescriptorDatabase::new(source_tree.as_mut());
    let fds = db
        .as_mut()
        .build_file_descriptor_set(&[Path::new(&primary_subject.name)])?;

    // Ensure there is exactly one message in the file.
    let primary_fd = fds.file(0);
    let message_name = match primary_fd.message_type_size() {
        1 => String::from_utf8_lossy(primary_fd.message_type(0).name()).into_owned(),
        0 => bail_unsupported!(9598, "Protobuf schemas with no messages"),
        _ => bail_unsupported!(9598, "Protobuf schemas with multiple messages"),
    };

    // Encode the file descriptor set into a SQL byte string.
    let mut schema = String::new();
    strconv::format_bytes(&mut schema, &fds.serialize()?);

    Ok(CsrSeedProtobufSchema {
        schema,
        message_name,
    })
}

/// Makes an always-valid AWS API call to perform a basic sanity check of
/// whether the specified AWS configuration is valid.
async fn validate_aws_credentials(
    config: &AwsConfig,
    external_id_prefix: Option<&AwsExternalIdPrefix>,
) -> Result<(), anyhow::Error> {
    let config = config.load(external_id_prefix, None).await;
    let sts_client = aws_sdk_sts::Client::new(&config);
    let _ = sts_client
        .get_caller_identity()
        .send()
        .await
        .context("Unable to validate AWS credentials")?;
    Ok(())
}
