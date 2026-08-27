//! Kubernetes API and reconciliation logic for a yesnodb leader/follower cluster.
//!
//! Automatic promotion is deliberately fail-closed. The controller first scales
//! the former primary to zero and confirms that no matching Pod remains before it
//! sends the durable promotion command. If Kubernetes cannot prove that fence,
//! the cluster stays unavailable instead of risking two writable leaders.

mod api;
mod controller;
mod resources;

pub use api::{
    CertManagerSpec, ConfigSpec, FailoverStage, FailoverStatus, IssuerReference,
    SecretKeyReference, StorageSpec, YesnoCluster, YesnoClusterPhase, YesnoClusterSpec,
    YesnoClusterStatus,
};
pub use controller::{run, Context, Error};

use kube::CustomResourceExt;

/// Render the CustomResourceDefinition installed by the deployment manifests.
pub fn crd_yaml() -> Result<String, serde_yaml::Error> {
    serde_yaml::to_string(&YesnoCluster::crd())
}

#[cfg(test)]
mod tests {
    #[test]
    fn checked_in_crd_matches_the_rust_api() {
        assert_eq!(
            super::crd_yaml().unwrap(),
            include_str!("../deploy/crd.yaml")
        );
    }

    #[test]
    fn example_manifest_deserializes_as_the_custom_resource() {
        let cluster: super::YesnoCluster =
            serde_yaml::from_str(include_str!("../deploy/example.yaml")).unwrap();
        assert_eq!(cluster.metadata.name.as_deref(), Some("search"));
        assert!(cluster.spec.config.cert_manager.is_some());
        assert!(!cluster.spec.config.allow_insecure);
    }

    #[test]
    fn operator_manifest_contains_five_valid_yaml_documents() {
        let documents = serde_yaml::Deserializer::from_str(include_str!("../deploy/operator.yaml"));
        assert_eq!(documents.count(), 5);
    }
}
