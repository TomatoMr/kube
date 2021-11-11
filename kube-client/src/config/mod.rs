//! Kubernetes configuration objects from `~/.kube/config`, `$KUBECONFIG`, or the [cluster environment](https://kubernetes.io/docs/tasks/access-application-cluster/access-cluster/#accessing-the-api-from-a-pod).
//!
//! # Usage
//! The [`Config`] has several constructors plus logic to infer environment.
//!
//! Unless you have issues, prefer using [`Config::infer`], and pass it to a [`Client`][crate::Client].
use std::{path::PathBuf, time::Duration};

use thiserror::Error;

mod file_config;
mod file_loader;
mod incluster_config;

use file_loader::ConfigLoader;
pub use file_loader::KubeConfigOptions;
pub use incluster_config::Error as InClusterError;

/// Failed to infer config
#[derive(Error, Debug)]
#[error("failed to infer config: in-cluster: ({in_cluster}), kubeconfig: ({kubeconfig})")]
pub struct InferConfigError {
    in_cluster: InClusterError,
    // We can only pick one source, but the kubeconfig failure is more likely to be a user error
    #[source]
    kubeconfig: KubeconfigError,
}

/// Possible errors when loading kubeconfig
#[derive(Error, Debug)]
pub enum KubeconfigError {
    /// Failed to determine current context
    #[error("failed to determine current context")]
    CurrentContextNotSet,

    /// Kubeconfigs with mismatching kind cannot be merged
    #[error("kubeconfigs with mismatching kind cannot be merged")]
    KindMismatch,

    /// Kubeconfigs with mismatching api version cannot be merged
    #[error("kubeconfigs with mismatching api version cannot be merged")]
    ApiVersionMismatch,

    /// Failed to load current context
    #[error("failed to load current context: {0}")]
    LoadContext(String),

    /// Failed to load the cluster of context
    #[error("failed to load the cluster of context: {0}")]
    LoadClusterOfContext(String),

    /// Failed to find named user
    #[error("failed to find named user: {0}")]
    FindUser(String),

    /// Failed to find the path of kubeconfig
    #[error("failed to find the path of kubeconfig")]
    FindPath,

    /// Failed to read kubeconfig
    #[error("failed to read kubeconfig from '{1:?}': {0}")]
    ReadConfig(#[source] std::io::Error, PathBuf),

    /// Failed to parse kubeconfig YAML
    #[error("failed to parse kubeconfig YAML: {0}")]
    Parse(#[source] serde_yaml::Error),

    /// The structure of the parsed kubeconfig is invalid
    #[error("the structure of the parsed kubeconfig is invalid: {0}")]
    InvalidStructure(#[source] serde_yaml::Error),

    /// Failed to find a single YAML document in kubeconfig
    #[error("failed to find a single YAML document in kubeconfig: {0}")]
    EmptyConfig(PathBuf),

    /// Failed to parse cluster url
    #[error("failed to parse cluster url: {0}")]
    ParseClusterUrl(#[source] http::uri::InvalidUri),

    /// Failed to parse proxy url
    #[error("failed to parse proxy url: {0}")]
    ParseProxyUrl(#[source] http::uri::InvalidUri),

    /// Failed to load certificate authority
    #[error("failed to load certificate authority")]
    LoadCertificateAuthority(#[source] LoadDataError),

    /// Failed to load client certificate
    #[error("failed to load client certificate")]
    LoadClientCertificate(#[source] LoadDataError),

    /// Failed to load client key
    #[error("failed to load client key")]
    LoadClientKey(#[source] LoadDataError),

    /// Failed to parse PEM-encoded certificates
    #[error("failed to parse PEM-encoded certificates: {0}")]
    ParseCertificates(#[source] pem::PemError),
}

/// Errors from loading data from a base64 string or a file
#[derive(Debug, Error)]
pub enum LoadDataError {
    /// Failed to decode base64 data
    #[error("failed to decode base64 data: {0}")]
    DecodeBase64(#[source] base64::DecodeError),

    /// Failed to read file
    #[error("failed to read file '{1:?}': {0}")]
    ReadFile(#[source] std::io::Error, PathBuf),

    /// No base64 data or file path was provided
    #[error("no base64 data or file")]
    NoBase64DataOrFile,
}

/// Configuration object detailing things like cluster URL, default namespace, root certificates, and timeouts.
///
/// # Usage
/// Construct a [`Config`] instance by using one of the many constructors.
///
/// Prefer [`Config::infer`] unless you have particular issues, and avoid manually managing
/// the data in this struct unless you have particular needs. It exists to be consumed by the [`Client`][crate::Client].
///
/// If you are looking to parse the kubeconfig found in a user's home directory see [`Kubeconfig`](crate::config::Kubeconfig).
#[cfg_attr(docsrs, doc(cfg(feature = "config")))]
#[derive(Debug, Clone)]
pub struct Config {
    /// The configured cluster url
    pub cluster_url: http::Uri,
    /// The configured default namespace
    pub default_namespace: String,
    /// The configured root certificate
    pub root_cert: Option<Vec<Vec<u8>>>,
    /// Timeout for calls to the Kubernetes API.
    ///
    /// A value of `None` means no timeout
    pub timeout: Option<std::time::Duration>,
    /// Whether to accept invalid ceritifacts
    pub accept_invalid_certs: bool,
    // TODO should keep client key and certificate separate. It's split later anyway.
    /// Client certificate and private key in PEM.
    pub(crate) identity_pem: Option<Vec<u8>>,
    /// Stores information to tell the cluster who you are.
    pub(crate) auth_info: AuthInfo,
    // TODO Actually support proxy or create an example with custom client
    /// Optional proxy URL.
    pub proxy_url: Option<http::Uri>,
}

impl Config {
    /// Construct a new config where only the `cluster_url` is set by the user.
    /// and everything else receives a default value.
    ///
    /// Most likely you want to use [`Config::infer`] to infer the config from
    /// the environment.
    pub fn new(cluster_url: http::Uri) -> Self {
        Self {
            cluster_url,
            default_namespace: String::from("default"),
            root_cert: None,
            timeout: Some(DEFAULT_TIMEOUT),
            accept_invalid_certs: false,
            identity_pem: None,
            auth_info: AuthInfo::default(),
            proxy_url: None,
        }
    }

    /// Infer the configuration from the environment
    ///
    /// Done by attempting to load in-cluster environment variables first, and
    /// then if that fails, trying the local kubeconfig.
    ///
    /// Fails if inference from both sources fails
    pub async fn infer() -> Result<Self, InferConfigError> {
        match Self::from_cluster_env() {
            Err(in_cluster_err) => {
                tracing::trace!("No in-cluster config found: {}", in_cluster_err);
                tracing::trace!("Falling back to local kubeconfig");
                let config = Self::from_kubeconfig(&KubeConfigOptions::default())
                    .await
                    .map_err(|kubeconfig_err| InferConfigError {
                        in_cluster: in_cluster_err,
                        kubeconfig: kubeconfig_err,
                    })?;

                Ok(config)
            }
            Ok(success) => Ok(success),
        }
    }

    /// Create configuration from the cluster's environment variables
    ///
    /// This follows the standard [API Access from a Pod](https://kubernetes.io/docs/tasks/access-application-cluster/access-cluster/#accessing-the-api-from-a-pod)
    /// and relies on you having the service account's token mounted,
    /// as well as having given the service account rbac access to do what you need.
    pub fn from_cluster_env() -> Result<Self, InClusterError> {
        let cluster_url = if cfg!(feature = "rustls-tls") {
            // try rolling out new method for rustls which does not support ip based urls anyway
            // see https://github.com/kube-rs/kube-rs/issues/587
            incluster_config::kube_dns()
        } else {
            incluster_config::kube_server()?
        };

        let default_namespace = incluster_config::load_default_ns()?;
        let root_cert = incluster_config::load_cert()?;
        let token = incluster_config::load_token()?;

        Ok(Self {
            cluster_url,
            default_namespace,
            root_cert: Some(root_cert),
            timeout: Some(DEFAULT_TIMEOUT),
            accept_invalid_certs: false,
            identity_pem: None,
            auth_info: AuthInfo {
                token: Some(token),
                ..Default::default()
            },
            proxy_url: None,
        })
    }

    /// Create configuration from the default local config file
    ///
    /// This will respect the `$KUBECONFIG` evar, but otherwise default to `~/.kube/config`.
    /// You can also customize what context/cluster/user you want to use here,
    /// but it will default to the current-context.
    pub async fn from_kubeconfig(options: &KubeConfigOptions) -> Result<Self, KubeconfigError> {
        let loader = ConfigLoader::new_from_options(options).await?;
        Self::new_from_loader(loader).await
    }

    /// Create configuration from a [`Kubeconfig`] struct
    ///
    /// This bypasses kube's normal config parsing to obtain custom functionality.
    pub async fn from_custom_kubeconfig(
        kubeconfig: Kubeconfig,
        options: &KubeConfigOptions,
    ) -> Result<Self, KubeconfigError> {
        let loader = ConfigLoader::new_from_kubeconfig(kubeconfig, options).await?;
        Self::new_from_loader(loader).await
    }

    async fn new_from_loader(loader: ConfigLoader) -> Result<Self, KubeconfigError> {
        let cluster_url = loader
            .cluster
            .server
            .parse::<http::Uri>()
            .map_err(KubeconfigError::ParseClusterUrl)?;

        let default_namespace = loader
            .current_context
            .namespace
            .clone()
            .unwrap_or_else(|| String::from("default"));

        let mut accept_invalid_certs = false;
        let mut root_cert = None;
        let mut identity_pem = None;

        if let Some(ca_bundle) = loader.ca_bundle()? {
            for ca in &ca_bundle {
                accept_invalid_certs = hacky_cert_lifetime_for_macos(ca);
            }
            root_cert = Some(ca_bundle);
        }

        // REVIEW Changed behavior. This no longer fails with invalid data in PEM.
        match loader.identity_pem() {
            Ok(id) => identity_pem = Some(id),
            Err(e) => {
                tracing::debug!("failed to load client identity from kubeconfig: {}", e);
                // last resort only if configs ask for it, and no client certs
                if let Some(true) = loader.cluster.insecure_skip_tls_verify {
                    accept_invalid_certs = true;
                }
            }
        }

        Ok(Self {
            cluster_url,
            default_namespace,
            root_cert,
            timeout: Some(DEFAULT_TIMEOUT),
            accept_invalid_certs,
            identity_pem,
            proxy_url: loader.proxy_url()?,
            auth_info: loader.user,
        })
    }
}

fn certs(data: &[u8]) -> Result<Vec<Vec<u8>>, pem::PemError> {
    Ok(pem::parse_many(data)?
        .into_iter()
        .filter_map(|p| {
            if p.tag == "CERTIFICATE" {
                Some(p.contents)
            } else {
                None
            }
        })
        .collect::<Vec<_>>())
}

// https://github.com/kube-rs/kube-rs/issues/146#issuecomment-590924397
/// Default Timeout
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(295);

// temporary catalina hack for openssl only
#[cfg(all(target_os = "macos", feature = "native-tls"))]
fn hacky_cert_lifetime_for_macos(ca: &[u8]) -> bool {
    use openssl::x509::X509;
    let ca = X509::from_der(ca).expect("valid der is a der");
    ca.not_before()
        .diff(ca.not_after())
        .map(|d| d.days.abs() > 824)
        .unwrap_or(false)
}

#[cfg(any(not(target_os = "macos"), not(feature = "native-tls")))]
fn hacky_cert_lifetime_for_macos(_: &[u8]) -> bool {
    false
}

// Expose raw config structs
pub use file_config::{
    AuthInfo, AuthProviderConfig, Cluster, Context, ExecConfig, Kubeconfig, NamedAuthInfo, NamedCluster,
    NamedContext, NamedExtension, Preferences,
};


#[cfg(test)]
mod tests {
    #[cfg(not(feature = "client"))] // want to ensure this works without client features
    #[tokio::test]
    async fn config_loading_on_small_feature_set() {
        use super::Config;
        let cfgraw = r#"
        apiVersion: v1
        clusters:
        - cluster:
            certificate-authority-data: aGVsbG8K
            server: https://0.0.0.0:6443
          name: k3d-test
        contexts:
        - context:
            cluster: k3d-test
            user: admin@k3d-test
          name: k3d-test
        current-context: k3d-test
        kind: Config
        preferences: {}
        users:
        - name: admin@k3d-test
          user:
            client-certificate-data: aGVsbG8K
            client-key-data: aGVsbG8K
        "#;
        let file = tempfile::NamedTempFile::new().expect("create config tempfile");
        std::fs::write(file.path(), cfgraw).unwrap();
        std::env::set_var("KUBECONFIG", file.path());
        let kubeconfig = Config::infer().await.unwrap();
        assert_eq!(kubeconfig.cluster_url, "https://0.0.0.0:6443/");
    }
}
