// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Connection types.

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use futures::executor::block_on;
use proptest::prelude::{any, Arbitrary, BoxedStrategy, Strategy};
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};
use tokio_postgres::config::SslMode;
use url::Url;

use mz_ccsr::tls::{Certificate, Identity};
use mz_kafka_util::KafkaAddrs;
use mz_proto::tokio_postgres::any_ssl_mode;
use mz_proto::{IntoRustIfSome, ProtoType, RustType, TryFromProtoError};
use mz_repr::url::any_url;
use mz_repr::GlobalId;
use mz_secrets::SecretsReader;
use mz_sql_parser::ast::KafkaConnectionOptionName;

use crate::types::connections::aws::AwsExternalIdPrefix;

pub mod aws;

include!(concat!(env!("OUT_DIR"), "/mz_storage.types.connections.rs"));

#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum StringOrSecret {
    String(String),
    Secret(GlobalId),
}

impl StringOrSecret {
    /// Gets the value as a string, reading the secret if necessary.
    pub async fn get_string(&self, secrets_reader: &dyn SecretsReader) -> anyhow::Result<String> {
        match self {
            StringOrSecret::String(s) => Ok(s.clone()),
            StringOrSecret::Secret(id) => secrets_reader.read_string(*id).await,
        }
    }

    /// Asserts that this string or secret is a string and returns its contents.
    pub fn unwrap_string(&self) -> &str {
        match self {
            StringOrSecret::String(s) => s,
            StringOrSecret::Secret(_) => panic!("StringOrSecret::unwrap_string called on a secret"),
        }
    }

    /// Asserts that this string or secret is a secret and returns its global
    /// ID.
    pub fn unwrap_secret(&self) -> GlobalId {
        match self {
            StringOrSecret::String(_) => panic!("StringOrSecret::unwrap_secret called on a string"),
            StringOrSecret::Secret(id) => *id,
        }
    }
}

impl RustType<ProtoStringOrSecret> for StringOrSecret {
    fn into_proto(&self) -> ProtoStringOrSecret {
        use proto_string_or_secret::Kind;
        ProtoStringOrSecret {
            kind: Some(match self {
                StringOrSecret::String(s) => Kind::String(s.clone()),
                StringOrSecret::Secret(id) => Kind::Secret(id.into_proto()),
            }),
        }
    }

    fn from_proto(proto: ProtoStringOrSecret) -> Result<Self, TryFromProtoError> {
        use proto_string_or_secret::Kind;
        let kind = proto
            .kind
            .ok_or_else(|| TryFromProtoError::missing_field("ProtoStringOrSecret::kind"))?;
        Ok(match kind {
            Kind::String(s) => StringOrSecret::String(s),
            Kind::Secret(id) => StringOrSecret::Secret(GlobalId::from_proto(id)?),
        })
    }
}

impl<V: std::fmt::Display> From<V> for StringOrSecret {
    fn from(v: V) -> StringOrSecret {
        StringOrSecret::String(format!("{}", v))
    }
}

/// Extra context to pass through when instantiating a connection for a source
/// or sink.
///
/// Should be kept cheaply cloneable.
#[derive(Debug, Clone)]
pub struct ConnectionContext {
    /// The level for librdkafka's logs.
    pub librdkafka_log_level: tracing::Level,
    /// A prefix for an external ID to use for all AWS AssumeRole operations.
    pub aws_external_id_prefix: Option<AwsExternalIdPrefix>,
    /// A secrets reader.
    pub secrets_reader: Arc<dyn SecretsReader>,
}

impl ConnectionContext {
    /// Constructs a new connection context from command line arguments.
    ///
    /// **WARNING:** it is critical for security that the `aws_external_id` be
    /// provided by the operator of the Materialize service (i.e., via a CLI
    /// argument or environment variable) and not the end user of Materialize
    /// (e.g., via a configuration option in a SQL statement). See
    /// [`AwsExternalIdPrefix`] for details.
    pub fn from_cli_args(
        filter: &tracing_subscriber::filter::Targets,
        aws_external_id_prefix: Option<String>,
        secrets_reader: Arc<dyn SecretsReader>,
    ) -> ConnectionContext {
        ConnectionContext {
            librdkafka_log_level: mz_ore::tracing::target_level(filter, "librdkafka"),
            aws_external_id_prefix: aws_external_id_prefix.map(AwsExternalIdPrefix),
            secrets_reader,
        }
    }

    /// Constructs a new connection context for usage in tests.
    pub fn for_tests(secrets_reader: Arc<dyn SecretsReader>) -> ConnectionContext {
        ConnectionContext {
            librdkafka_log_level: tracing::Level::INFO,
            aws_external_id_prefix: None,
            secrets_reader,
        }
    }
}

#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Connection {
    Kafka(KafkaConnection),
    Csr(CsrConnection),
    Postgres(PostgresConnection),
    Ssh(SshConnection),
}

#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct KafkaTlsConfig {
    pub identity: Option<TlsIdentity>,
    pub root_cert: Option<StringOrSecret>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SaslConfig {
    pub mechanisms: String,
    pub username: StringOrSecret,
    pub password: GlobalId,
    pub tls_root_cert: Option<StringOrSecret>,
}

impl Arbitrary for SaslConfig {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            any::<String>(),
            StringOrSecret::arbitrary(),
            GlobalId::arbitrary(),
            proptest::option::of(any::<StringOrSecret>()),
        )
            .prop_map(
                |(mechanisms, username, password, tls_root_cert)| SaslConfig {
                    mechanisms,
                    username,
                    password,
                    tls_root_cert,
                },
            )
            .boxed()
    }
}

#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum KafkaSecurity {
    Tls(KafkaTlsConfig),
    Sasl(SaslConfig),
}

impl From<KafkaTlsConfig> for KafkaSecurity {
    fn from(c: KafkaTlsConfig) -> Self {
        KafkaSecurity::Tls(c)
    }
}

impl From<SaslConfig> for KafkaSecurity {
    fn from(c: SaslConfig) -> Self {
        KafkaSecurity::Sasl(c)
    }
}

/// Meant to create an equivalence function between enum-named options and
/// their free-form `String` counterparts.
pub trait ConfigKey {
    fn config_key(&self) -> String;
}

impl ConfigKey for KafkaConnectionOptionName {
    fn config_key(&self) -> String {
        use KafkaConnectionOptionName::*;
        match self {
            Broker | Brokers => "bootstrap.servers",
            SslKey => "ssl.key.pem",
            SslCertificate => "ssl.certificate.pem",
            SslCertificateAuthority => "ssl.ca.pem",
            SaslMechanisms => "sasl.mechanisms",
            SaslUsername => "sasl.username",
            SaslPassword => "sasl.password",
        }
        .to_string()
    }
}

#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct KafkaConnection {
    pub brokers: Vec<String>,
    pub security: Option<KafkaSecurity>,
}

impl From<KafkaConnection> for BTreeMap<String, StringOrSecret> {
    fn from(v: KafkaConnection) -> Self {
        use KafkaConnectionOptionName::*;
        let mut r = BTreeMap::new();
        r.insert("bootstrap.servers".into(), v.brokers.join(",").into());
        match v.security {
            Some(KafkaSecurity::Tls(KafkaTlsConfig {
                root_cert,
                identity,
            })) => {
                r.insert("security.protocol".into(), "SSL".into());
                if let Some(root_cert) = root_cert {
                    r.insert(SslCertificateAuthority.config_key(), root_cert);
                }
                if let Some(identity) = identity {
                    r.insert(SslKey.config_key(), StringOrSecret::Secret(identity.key));
                    r.insert(SslCertificate.config_key(), identity.cert);
                }
            }
            Some(KafkaSecurity::Sasl(SaslConfig {
                mechanisms,
                username,
                password,
                tls_root_cert: certificate_authority,
            })) => {
                r.insert("security.protocol".into(), "SASL_SSL".into());
                r.insert(
                    SaslMechanisms.config_key(),
                    StringOrSecret::String(mechanisms),
                );
                r.insert(SaslUsername.config_key(), username);
                r.insert(SaslPassword.config_key(), StringOrSecret::Secret(password));
                if let Some(certificate_authority) = certificate_authority {
                    r.insert(SslCertificateAuthority.config_key(), certificate_authority);
                }
            }
            None => {}
        }

        r
    }
}

impl TryFrom<&mut BTreeMap<String, StringOrSecret>> for KafkaConnection {
    type Error = anyhow::Error;
    /// Extracts only the options necessary to create a `KafkaConnection` from
    /// a `BTreeMap<String, StringOrSecret>`, and returns the remaining
    /// options.
    ///
    /// # Panics
    /// - If `value` was not sufficiently or incorrectly type checked and
    ///   parameters expected to reference objects (i.e. secrets) are instead
    ///   `String`s, or vice versa.
    fn try_from(map: &mut BTreeMap<String, StringOrSecret>) -> Result<Self, Self::Error> {
        use KafkaConnectionOptionName::*;

        let key_or_err = |config: &str,
                          map: &mut BTreeMap<String, StringOrSecret>,
                          key: KafkaConnectionOptionName| {
            map.remove(&key.config_key()).ok_or_else(|| {
                anyhow!(
                    "invalid {} config: missing {} ({})",
                    config.to_uppercase(),
                    key,
                    key.config_key(),
                )
            })
        };

        let security = if let Some(v) = map.remove("security.protocol") {
            match v.unwrap_string().to_lowercase().as_str() {
                config @ "ssl" => Some(KafkaSecurity::Tls(KafkaTlsConfig {
                    identity: Some(TlsIdentity {
                        key: key_or_err(config, map, SslKey)?.unwrap_secret(),
                        cert: key_or_err(config, map, SslCertificate)?,
                    }),
                    root_cert: map.remove(&SslCertificateAuthority.config_key()),
                })),
                config @ "sasl_ssl" => Some(KafkaSecurity::Sasl(SaslConfig {
                    mechanisms: key_or_err(config, map, SaslMechanisms)?
                        .unwrap_string()
                        .to_string(),
                    username: key_or_err(config, map, SaslUsername)?,
                    password: key_or_err(config, map, SaslPassword)?.unwrap_secret(),
                    tls_root_cert: map.remove(&SslCertificateAuthority.config_key()),
                })),
                o => bail!("unsupported security.protocol: {}", o),
            }
        } else {
            None
        };

        let brokers = match map.remove(&Broker.config_key()) {
            Some(v) => KafkaAddrs::from_str(&v.unwrap_string())?
                .to_string()
                .split(',')
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            None => bail!("must specify {}", Broker.config_key()),
        };

        Ok(KafkaConnection { brokers, security })
    }
}

impl RustType<ProtoKafkaConnectionTlsConfig> for KafkaTlsConfig {
    fn into_proto(&self) -> ProtoKafkaConnectionTlsConfig {
        ProtoKafkaConnectionTlsConfig {
            identity: self.identity.into_proto(),
            root_cert: self.root_cert.into_proto(),
        }
    }

    fn from_proto(proto: ProtoKafkaConnectionTlsConfig) -> Result<Self, TryFromProtoError> {
        Ok(KafkaTlsConfig {
            root_cert: proto.root_cert.into_rust()?,
            identity: proto.identity.into_rust()?,
        })
    }
}

impl RustType<ProtoKafkaConnectionSaslConfig> for SaslConfig {
    fn into_proto(&self) -> ProtoKafkaConnectionSaslConfig {
        ProtoKafkaConnectionSaslConfig {
            mechanisms: self.mechanisms.into_proto(),
            username: Some(self.username.into_proto()),
            password: Some(self.password.into_proto()),
            tls_root_cert: self.tls_root_cert.into_proto(),
        }
    }

    fn from_proto(proto: ProtoKafkaConnectionSaslConfig) -> Result<Self, TryFromProtoError> {
        Ok(SaslConfig {
            mechanisms: proto.mechanisms,
            username: proto
                .username
                .into_rust_if_some("ProtoKafkaConnectionSaslConfig::username")?,
            password: proto
                .password
                .into_rust_if_some("ProtoKafkaConnectionSaslConfig::password")?,
            tls_root_cert: proto.tls_root_cert.into_rust()?,
        })
    }
}

impl RustType<ProtoKafkaConnectionSecurity> for KafkaSecurity {
    fn into_proto(&self) -> ProtoKafkaConnectionSecurity {
        use proto_kafka_connection_security::Kind;
        ProtoKafkaConnectionSecurity {
            kind: Some(match self {
                KafkaSecurity::Tls(config) => Kind::Tls(config.into_proto()),
                KafkaSecurity::Sasl(config) => Kind::Sasl(config.into_proto()),
            }),
        }
    }

    fn from_proto(proto: ProtoKafkaConnectionSecurity) -> Result<Self, TryFromProtoError> {
        use proto_kafka_connection_security::Kind;
        let kind = proto.kind.ok_or_else(|| {
            TryFromProtoError::missing_field("ProtoKafkaConnectionSecurity::kind")
        })?;
        Ok(match kind {
            Kind::Tls(s) => KafkaSecurity::Tls(KafkaTlsConfig::from_proto(s)?),
            Kind::Sasl(s) => KafkaSecurity::Sasl(SaslConfig::from_proto(s)?),
        })
    }
}

impl RustType<ProtoKafkaConnection> for KafkaConnection {
    fn into_proto(&self) -> ProtoKafkaConnection {
        ProtoKafkaConnection {
            brokers: self.brokers.into_proto(),
            security: self.security.into_proto(),
        }
    }

    fn from_proto(proto: ProtoKafkaConnection) -> Result<Self, TryFromProtoError> {
        Ok(KafkaConnection {
            brokers: proto.brokers,
            security: proto.security.into_rust()?,
        })
    }
}

/// A connection to a Confluent Schema Registry.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct CsrConnection {
    /// The URL of the schema registry.
    pub url: Url,
    /// Trusted root TLS certificate in PEM format.
    pub tls_root_cert: Option<StringOrSecret>,
    /// An optional TLS client certificate for authentication with the schema
    /// registry.
    pub tls_identity: Option<TlsIdentity>,
    /// Optional HTTP authentication credentials for the schema registry.
    pub http_auth: Option<CsrConnectionHttpAuth>,
}

impl CsrConnection {
    /// Constructs a schema registry client from the connection.
    pub async fn connect(
        &self,
        secrets_reader: &dyn SecretsReader,
    ) -> Result<mz_ccsr::Client, anyhow::Error> {
        let mut client_config = mz_ccsr::ClientConfig::new(self.url.clone());
        if let Some(root_cert) = &self.tls_root_cert {
            let root_cert = root_cert.get_string(secrets_reader).await?;
            let root_cert = Certificate::from_pem(&root_cert.as_bytes())?;
            client_config = client_config.add_root_certificate(root_cert);
        }

        if let Some(tls_identity) = &self.tls_identity {
            let key = secrets_reader.read_string(tls_identity.key).await?;
            let cert = tls_identity.cert.get_string(secrets_reader).await?;
            // `reqwest` expects identity `pem` files to contain one key and
            // at least one certificate.
            let mut buf = Vec::new();
            buf.extend(key.as_bytes());
            buf.push(b'\n');
            buf.extend(cert.as_bytes());
            let ident = Identity::from_pem(&buf)?;
            client_config = client_config.identity(ident);
        }

        if let Some(http_auth) = &self.http_auth {
            let username = http_auth.username.get_string(secrets_reader).await?;
            let password = match http_auth.password {
                None => None,
                Some(password) => Some(secrets_reader.read_string(password).await?),
            };
            client_config = client_config.auth(username, password);
        }

        client_config.build()
    }
}

impl RustType<ProtoCsrConnection> for CsrConnection {
    fn into_proto(&self) -> ProtoCsrConnection {
        ProtoCsrConnection {
            url: Some(self.url.into_proto()),
            tls_root_cert: self.tls_root_cert.into_proto(),
            tls_identity: self.tls_identity.into_proto(),
            http_auth: self.http_auth.into_proto(),
        }
    }

    fn from_proto(proto: ProtoCsrConnection) -> Result<Self, TryFromProtoError> {
        Ok(CsrConnection {
            url: proto.url.into_rust_if_some("ProtoCsrConnection::url")?,
            tls_root_cert: proto.tls_root_cert.into_rust()?,
            tls_identity: proto.tls_identity.into_rust()?,
            http_auth: proto.http_auth.into_rust()?,
        })
    }
}

impl Arbitrary for CsrConnection {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            any_url(),
            any::<Option<StringOrSecret>>(),
            any::<Option<TlsIdentity>>(),
            any::<Option<CsrConnectionHttpAuth>>(),
        )
            .prop_map(
                |(url, tls_root_cert, tls_identity, http_auth)| CsrConnection {
                    url,
                    tls_root_cert,
                    tls_identity,
                    http_auth,
                },
            )
            .boxed()
    }
}

/// A TLS key pair used for client identity.
#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct TlsIdentity {
    /// The client's TLS public certificate in PEM format.
    pub cert: StringOrSecret,
    /// The ID of the secret containing the client's TLS private key in PEM
    /// format.
    pub key: GlobalId,
}

impl RustType<ProtoTlsIdentity> for TlsIdentity {
    fn into_proto(&self) -> ProtoTlsIdentity {
        ProtoTlsIdentity {
            cert: Some(self.cert.into_proto()),
            key: Some(self.key.into_proto()),
        }
    }

    fn from_proto(proto: ProtoTlsIdentity) -> Result<Self, TryFromProtoError> {
        Ok(TlsIdentity {
            cert: proto.cert.into_rust_if_some("ProtoTlsIdentity::cert")?,
            key: proto.key.into_rust_if_some("ProtoTlsIdentity::key")?,
        })
    }
}

/// HTTP authentication credentials in a [`CsrConnection`].
#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct CsrConnectionHttpAuth {
    /// The username.
    pub username: StringOrSecret,
    /// The ID of the secret containing the password, if any.
    pub password: Option<GlobalId>,
}

impl RustType<ProtoCsrConnectionHttpAuth> for CsrConnectionHttpAuth {
    fn into_proto(&self) -> ProtoCsrConnectionHttpAuth {
        ProtoCsrConnectionHttpAuth {
            username: Some(self.username.into_proto()),
            password: self.password.into_proto(),
        }
    }

    fn from_proto(proto: ProtoCsrConnectionHttpAuth) -> Result<Self, TryFromProtoError> {
        Ok(CsrConnectionHttpAuth {
            username: proto
                .username
                .into_rust_if_some("ProtoCsrConnectionHttpAuth::username")?,
            password: proto.password.into_rust()?,
        })
    }
}

/// Propagates appropriate configuration options from `kafka_connection` and
/// `options` into `config`, ignoring any options identified in
/// `drop_option_keys`.
///
/// Note that this:
/// - Performs blocking reads when extracting SECRETS.
/// - Does not ensure that the keys from the Kafka connection and
///   additional options are disjoint.
pub fn populate_client_config(
    kafka_connection: KafkaConnection,
    options: &BTreeMap<String, StringOrSecret>,
    drop_option_keys: HashSet<&'static str>,
    config: &mut rdkafka::ClientConfig,
    secrets_reader: &dyn SecretsReader,
) {
    let config_options: BTreeMap<String, StringOrSecret> = kafka_connection.into();
    for (k, v) in options.iter().chain(config_options.iter()) {
        if !drop_option_keys.contains(k.as_str()) {
            config.set(
                k,
                block_on(v.get_string(secrets_reader))
                    .expect("reading kafka secret unexpectedly failed"),
            );
        }
    }
}

/// Provides cleaner access to the `populate_client_config` implementation for
/// structs.
pub trait PopulateClientConfig {
    fn kafka_connection(&self) -> &KafkaConnection;
    fn options(&self) -> &BTreeMap<String, StringOrSecret>;
    fn drop_option_keys() -> HashSet<&'static str> {
        HashSet::new()
    }
    fn populate_client_config(
        &self,
        config: &mut rdkafka::ClientConfig,
        secrets_reader: &dyn SecretsReader,
    ) {
        populate_client_config(
            self.kafka_connection().clone(),
            self.options(),
            Self::drop_option_keys(),
            config,
            secrets_reader,
        )
    }
}

/// A connection to a PostgreSQL server.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PostgresConnection {
    /// The hostname of the server.
    pub host: String,
    /// The port of the server.
    pub port: u16,
    /// The name of the database to connect to.
    pub database: String,
    /// The username to authenticate as.
    pub user: StringOrSecret,
    /// An optional password for authentication.
    pub password: Option<GlobalId>,
    /// An optional named SSH tunnel connection.
    pub ssh_tunnel: Option<String>,
    /// Whether to use TLS for encryption, authentication, or both.
    pub tls_mode: SslMode,
    /// An optional root TLS certificate in PEM format, to verify the server's
    /// identity.
    pub tls_root_cert: Option<StringOrSecret>,
    /// An optional TLS client certificate for authentication.
    pub tls_identity: Option<TlsIdentity>,
}

impl PostgresConnection {
    pub async fn config(
        &self,
        secrets_reader: &dyn mz_secrets::SecretsReader,
    ) -> Result<tokio_postgres::Config, anyhow::Error> {
        let user = self.user.get_string(secrets_reader).await?;
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .dbname(&self.database)
            .user(&user)
            .ssl_mode(self.tls_mode);
        if let Some(password) = self.password {
            let password = secrets_reader.read_string(password).await?;
            config.password(password);
        }
        if let Some(tls_root_cert) = &self.tls_root_cert {
            let tls_root_cert = tls_root_cert.get_string(secrets_reader).await?;
            config.ssl_root_cert(tls_root_cert.as_bytes());
        }
        if let Some(tls_identity) = &self.tls_identity {
            let cert = tls_identity.cert.get_string(secrets_reader).await?;
            let key = secrets_reader.read_string(tls_identity.key).await?;
            config.ssl_cert(cert.as_bytes()).ssl_key(key.as_bytes());
        }
        Ok(config)
    }
}

impl RustType<ProtoPostgresConnection> for PostgresConnection {
    fn into_proto(&self) -> ProtoPostgresConnection {
        ProtoPostgresConnection {
            host: self.host.into_proto(),
            port: self.port.into_proto(),
            database: self.database.into_proto(),
            user: Some(self.user.into_proto()),
            password: self.password.into_proto(),
            ssh_tunnel: self.ssh_tunnel.into_proto(),
            tls_mode: Some(self.tls_mode.into_proto()),
            tls_root_cert: self.tls_root_cert.into_proto(),
            tls_identity: self.tls_identity.into_proto(),
        }
    }

    fn from_proto(proto: ProtoPostgresConnection) -> Result<Self, TryFromProtoError> {
        Ok(PostgresConnection {
            host: proto.host,
            port: proto.port.into_rust()?,
            database: proto.database,
            user: proto
                .user
                .into_rust_if_some("ProtoPostgresConnection::user")?,
            password: proto.password.into_rust()?,
            ssh_tunnel: proto.ssh_tunnel.into_rust()?,
            tls_mode: proto
                .tls_mode
                .into_rust_if_some("ProtoPostgresConnection::tls_mode")?,
            tls_root_cert: proto.tls_root_cert.into_rust()?,
            tls_identity: proto.tls_identity.into_rust()?,
        })
    }
}

impl Arbitrary for PostgresConnection {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            any::<String>(),
            any::<u16>(),
            any::<String>(),
            any::<StringOrSecret>(),
            any::<Option<GlobalId>>(),
            any::<Option<String>>(),
            any_ssl_mode(),
            any::<Option<StringOrSecret>>(),
            any::<Option<TlsIdentity>>(),
        )
            .prop_map(
                |(
                    host,
                    port,
                    database,
                    user,
                    password,
                    ssh_tunnel,
                    tls_mode,
                    tls_root_cert,
                    tls_identity,
                )| {
                    PostgresConnection {
                        host,
                        port,
                        database,
                        user,
                        password,
                        ssh_tunnel,
                        tls_mode,
                        tls_root_cert,
                        tls_identity,
                    }
                },
            )
            .boxed()
    }
}

/// A connection to a SSH tunnel.
#[derive(Arbitrary, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SshConnection {
    pub host: String,
    pub port: i32,
    pub user: String,
    pub public_key: String,
    pub private_key: GlobalId,
}
