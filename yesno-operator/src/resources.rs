use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, HTTPGetAction,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, Probe, SecretVolumeSource, SecurityContext, Service, ServicePort, ServiceSpec,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Resource, ResourceExt};
use serde_json::json;

use crate::api::{SnapshotBackend, YesnoCluster};

pub(crate) const FIELD_MANAGER: &str = "yesno-operator";
pub(crate) const INSTANCE_LABEL: &str = "yesnodb.io/instance";
pub(crate) const ROLE_LABEL: &str = "yesnodb.io/role";
const CONFIG_PATH: &str = "/etc/yesno/yesnod.toml";
const DATA_PATH: &str = "/var/lib/yesno";
const TLS_PATH: &str = "/etc/yesno/tls";
const CA_PATH: &str = "/etc/yesno/ca/ca.crt";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstanceRole {
    Leader,
    Follower,
}

impl InstanceRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Leader => "leader",
            Self::Follower => "follower",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ConfigSource {
    Generated {
        name: String,
        body: String,
        tls: Option<TlsMount>,
    },
    Secret {
        name: String,
        identity: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct TlsMount {
    pub(crate) server_secret: String,
    pub(crate) ca_secret: String,
    pub(crate) ca_key: String,
    pub(crate) identity: String,
}

#[derive(Clone, Debug)]
pub(crate) struct TlsTopology {
    pub(crate) client_fingerprint: String,
    pub(crate) instance_fingerprints: Vec<String>,
    pub(crate) ca_secret: String,
    pub(crate) ca_key: String,
    pub(crate) identity: String,
}

pub(crate) fn base_name(cluster: &YesnoCluster) -> String {
    let original = cluster.name_any();
    let normalized = original.replace('.', "-");
    if normalized.len() <= 48 {
        return normalized;
    }

    let hash = crc32c::crc32c(normalized.as_bytes());
    let prefix = normalized[..39].trim_end_matches('-');
    format!("{prefix}-{hash:08x}")
}

pub(crate) fn instance_name(cluster: &YesnoCluster, instance: i32) -> String {
    format!("{}-{instance}", base_name(cluster))
}

pub(crate) fn client_certificate_name(cluster: &YesnoCluster) -> String {
    format!("{}-client-tls", base_name(cluster))
}

pub(crate) fn instance_certificate_name(cluster: &YesnoCluster, instance: i32) -> String {
    format!("{}-tls", instance_name(cluster, instance))
}

pub(crate) fn certificate_api_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(
        "cert-manager.io",
        "v1",
        "Certificate",
    ))
}

fn dns_names(name: &str, namespace: &str) -> Vec<String> {
    vec![
        name.into(),
        format!("{name}.{namespace}"),
        format!("{name}.{namespace}.svc"),
        format!("{name}.{namespace}.svc.cluster.local"),
    ]
}

pub(crate) fn certificate(
    cluster: &YesnoCluster,
    name: &str,
    secret_name: &str,
    dns: Vec<String>,
    usages: &[&str],
) -> DynamicObject {
    let namespace = cluster.namespace().expect("cluster has a namespace");
    let mut object = DynamicObject::new(name, &certificate_api_resource()).within(&namespace);
    object.metadata.labels = Some(labels(cluster));
    object.metadata.owner_references = Some(vec![owner(cluster)]);
    let cert_manager = cluster
        .spec
        .config
        .cert_manager
        .as_ref()
        .expect("certificate resources need certManager config");
    object.data = json!({
        "spec": {
            "secretName": secret_name,
            "secretTemplate": { "labels": labels(cluster) },
            "privateKey": { "rotationPolicy": "Always" },
            "dnsNames": dns,
            "usages": usages,
            "issuerRef": {
                "name": cert_manager.issuer_ref.name,
                "kind": cert_manager.issuer_ref.kind,
                "group": cert_manager.issuer_ref.group,
            }
        }
    });
    object
}

pub(crate) fn client_certificate(cluster: &YesnoCluster) -> DynamicObject {
    let name = client_certificate_name(cluster);
    let namespace = cluster.namespace().expect("cluster has a namespace");
    certificate(
        cluster,
        &name,
        &name,
        dns_names(&format!("{}-client", base_name(cluster)), &namespace),
        &["client auth"],
    )
}

pub(crate) fn instance_certificate(cluster: &YesnoCluster, instance: i32) -> DynamicObject {
    let name = instance_certificate_name(cluster, instance);
    let namespace = cluster.namespace().expect("cluster has a namespace");
    let mut dns = dns_names(&instance_name(cluster, instance), &namespace);
    for service in [
        format!("{}-rw", base_name(cluster)),
        format!("{}-ro", base_name(cluster)),
    ] {
        dns.extend(dns_names(&service, &namespace));
    }
    dns.sort();
    dns.dedup();
    certificate(cluster, &name, &name, dns, &["server auth", "client auth"])
}

pub(crate) fn labels(cluster: &YesnoCluster) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "yesnodb".into()),
        ("app.kubernetes.io/instance".into(), base_name(cluster)),
        ("app.kubernetes.io/managed-by".into(), FIELD_MANAGER.into()),
    ])
}

pub(crate) fn instance_labels(cluster: &YesnoCluster, instance: i32) -> BTreeMap<String, String> {
    let mut result = labels(cluster);
    result.insert(INSTANCE_LABEL.into(), instance.to_string());
    result
}

fn role_labels(
    cluster: &YesnoCluster,
    instance: i32,
    role: InstanceRole,
) -> BTreeMap<String, String> {
    let mut result = instance_labels(cluster, instance);
    result.insert(ROLE_LABEL.into(), role.as_str().into());
    result
}

pub(crate) fn config_source(
    cluster: &YesnoCluster,
    instance: i32,
    role: InstanceRole,
    secret_identity: Option<&str>,
    tls: Option<&TlsTopology>,
    volume_id: Option<&str>,
) -> ConfigSource {
    if let Some(secret_name) = &cluster.spec.config.secret_name {
        return ConfigSource::Secret {
            name: secret_name.clone(),
            identity: secret_identity.unwrap_or(secret_name).into(),
        };
    }

    let name = format!("{}-config", instance_name(cluster, instance));
    let snapshot = snapshot_section(cluster, volume_id);
    if let Some(tls) = tls {
        return secure_config_source(cluster, instance, role, name, tls, &snapshot);
    }
    let follower = if role == InstanceRole::Follower {
        format!(
            "\n[follower]\nleader = \"http://{}-rw:50052\"\nserve_reads = true\n",
            base_name(cluster)
        )
    } else {
        String::new()
    };
    let body = format!(
        "[server]\nrole = \"{}\"\ndata_dir = \"{DATA_PATH}\"\nshutdown_grace_secs = {}\n\n\
         [server.flight]\nlisten = \"0.0.0.0:50051\"\n\n\
         [server.control]\nlisten = \"0.0.0.0:50052\"\njournal_dir = \"{DATA_PATH}/control\"\n\n\
         [server.metrics]\nlisten = \"0.0.0.0:9750\"\n\n{}\
         [db]\nshards = {}\n\n\
         [[auth.rule]]\nprincipal = \"all\"\naddress = \"0.0.0.0/0\"\ncapability = \"control-read\"\naction = \"allow\"\n\n\
         [[auth.rule]]\nprincipal = \"all\"\naddress = \"0.0.0.0/0\"\ncapability = \"control-admin\"\naction = \"allow\"\n\n\
         [[auth.rule]]\nprincipal = \"all\"\naddress = \"0.0.0.0/0\"\ncapability = \"replication\"\naction = \"allow\"\n{}",
        role.as_str(),
        cluster.spec.shutdown_grace_secs,
        snapshot,
        cluster.spec.shards,
        follower
    );
    ConfigSource::Generated {
        name,
        body,
        tls: None,
    }
}

fn secure_config_source(
    cluster: &YesnoCluster,
    instance: i32,
    role: InstanceRole,
    name: String,
    tls: &TlsTopology,
    snapshot: &str,
) -> ConfigSource {
    let server_cert = format!("{TLS_PATH}/tls.crt");
    let server_key = format!("{TLS_PATH}/tls.key");
    let follower = if role == InstanceRole::Follower {
        format!(
            "\n[follower]\nleader = \"https://{}-rw:50052\"\nserve_reads = true\n\n\
             [follower.tls]\nca = \"{CA_PATH}\"\ncert = \"{server_cert}\"\nkey = \"{server_key}\"\n\
             domain = \"{}-rw\"\n",
            base_name(cluster),
            base_name(cluster),
        )
    } else {
        String::new()
    };
    let mut principals = format!(
        "[[auth.principal]]\nname = \"cluster-client\"\nrole = \"admin\"\ncert_sha256 = \"{}\"\n\n",
        tls.client_fingerprint
    );
    for (peer, fingerprint) in tls.instance_fingerprints.iter().enumerate() {
        principals.push_str(&format!(
            "[[auth.principal]]\nname = \"instance-{peer}\"\nrole = \"replica\"\ncert_sha256 = \"{fingerprint}\"\n\n"
        ));
    }
    let mut rules = String::new();
    for capability in ["control-read", "control-admin"] {
        rules.push_str(&format!(
            "[[auth.rule]]\nchannel = \"hostssl\"\nprincipal = \"cluster-client\"\naddress = \"0.0.0.0/0\"\ncapability = \"{capability}\"\naction = \"allow\"\n\n"
        ));
    }
    for peer in 0..tls.instance_fingerprints.len() {
        rules.push_str(&format!(
            "[[auth.rule]]\nchannel = \"hostssl\"\nprincipal = \"instance-{peer}\"\naddress = \"0.0.0.0/0\"\ncapability = \"replication\"\naction = \"allow\"\n\n"
        ));
    }
    let body = format!(
        "[server]\nrole = \"{}\"\ndata_dir = \"{DATA_PATH}\"\nshutdown_grace_secs = {}\n\n\
         [server.flight]\nlisten = \"0.0.0.0:50051\"\n\n\
         [server.flight.tls]\ncert = \"{server_cert}\"\nkey = \"{server_key}\"\nclient_ca = \"{CA_PATH}\"\nrequire_client_auth = true\n\n\
         [server.control]\nlisten = \"0.0.0.0:50052\"\njournal_dir = \"{DATA_PATH}/control\"\n\n\
         [server.control.tls]\ncert = \"{server_cert}\"\nkey = \"{server_key}\"\nclient_ca = \"{CA_PATH}\"\nrequire_client_auth = true\n\n\
         [server.metrics]\nlisten = \"0.0.0.0:9750\"\n\n{}\
         [db]\nshards = {}\n\n{}{}{}",
        role.as_str(),
        cluster.spec.shutdown_grace_secs,
        snapshot,
        cluster.spec.shards,
        principals,
        rules,
        follower,
    );
    ConfigSource::Generated {
        name,
        body,
        tls: Some(TlsMount {
            server_secret: instance_certificate_name(cluster, instance),
            ca_secret: tls.ca_secret.clone(),
            ca_key: tls.ca_key.clone(),
            identity: tls.identity.clone(),
        }),
    }
}

/// One TOML basic string.
///
/// Every value below comes from a `YesnoCluster` an ordinary namespace user
/// can write, and the result is a configuration file the daemon obeys, so a
/// value carrying a quote must not be able to close one and start a key of its
/// own. `validate` in the controller already refuses the characters that would
/// make this matter; escaping here means the rendering is safe on its own
/// terms rather than because a check elsewhere held.
fn toml_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            control if control.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", control as u32));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// `[server.snapshot]` for one instance, or nothing.
///
/// **Nothing is what a cluster gets on its first reconcile, by design.** The
/// EBS volume id is per instance and is read from the PersistentVolume the
/// claim bound to, and an EBS StorageClass normally binds
/// `WaitForFirstConsumer` -- the volume does not exist until the Pod this very
/// configuration belongs to has been scheduled. Refusing to generate anything
/// until the volume is known would therefore deadlock. The instance instead
/// starts on the portable provider and picks up the EBS backend on the
/// reconcile after its claim binds, which changes the config identity and
/// restarts the Pod exactly once. `.status.conditions` carries a
/// `SnapshotBackendReady` entry throughout, so the window is visible rather
/// than merely brief.
fn snapshot_section(cluster: &YesnoCluster, volume_id: Option<&str>) -> String {
    let snapshot = &cluster.spec.snapshot;
    let (SnapshotBackend::Ebs, Some(ebs), Some(volume_id)) =
        (snapshot.backend, snapshot.ebs.as_ref(), volume_id)
    else {
        return String::new();
    };

    // `materialization` is fixed rather than configurable: see `EbsSnapshotSpec`.
    // `source_mount` is the PVC's mount point, so the database directory is the
    // root of the volume being snapshotted and its subpath within it is empty.
    let mut section = format!(
        "[server.snapshot]\nbackend = \"ebs\"\nlease_ttl_secs = {}\n\n\
         [server.snapshot.ebs]\nmaterialization = \"deferred\"\nregion = {}\n\
         volume_id = {}\nsource_mount = {}\nfilesystem = {}\n\
         operation_timeout_secs = {}\n\n",
        snapshot.lease_ttl_secs,
        toml_string(&ebs.region),
        toml_string(volume_id),
        toml_string(DATA_PATH),
        toml_string(&ebs.filesystem),
        ebs.operation_timeout_secs,
    );
    if !ebs.resource_tags.is_empty() {
        section.push_str("[server.snapshot.ebs.resource_tags]\n");
        for (key, value) in &ebs.resource_tags {
            section.push_str(&format!("{} = {}\n", toml_string(key), toml_string(value)));
        }
        section.push('\n');
    }
    section
}

pub(crate) fn owner(cluster: &YesnoCluster) -> OwnerReference {
    cluster
        .controller_owner_ref(&())
        .expect("a persisted YesnoCluster has a UID")
}

pub(crate) fn config_map(
    cluster: &YesnoCluster,
    instance: i32,
    name: &str,
    body: &str,
) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: cluster.namespace(),
            labels: Some(instance_labels(cluster, instance)),
            owner_references: Some(vec![owner(cluster)]),
            ..Default::default()
        },
        data: Some(BTreeMap::from([("yesnod.toml".into(), body.into())])),
        ..Default::default()
    }
}

pub(crate) fn data_claim_name(cluster: &YesnoCluster, instance: i32) -> String {
    cluster
        .spec
        .storage
        .existing_claim
        .clone()
        .unwrap_or_else(|| format!("{}-data", instance_name(cluster, instance)))
}

pub(crate) fn persistent_volume_claim(
    cluster: &YesnoCluster,
    instance: i32,
) -> PersistentVolumeClaim {
    let name = data_claim_name(cluster, instance);
    let size = cluster
        .spec
        .storage
        .size
        .as_deref()
        .expect("validated managed storage has a size");
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: cluster.namespace(),
            labels: Some(instance_labels(cluster, instance)),
            annotations: Some(BTreeMap::from([(
                "yesnodb.io/retention".into(),
                "retained-after-cluster-deletion".into(),
            )])),
            // Deliberately no owner reference: deleting the CR must not delete data.
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".into()]),
            resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                requests: Some(BTreeMap::from([("storage".into(), Quantity(size.into()))])),
                ..Default::default()
            }),
            storage_class_name: cluster.spec.storage.storage_class_name.clone(),
            volume_mode: Some("Filesystem".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service_ports() -> Vec<ServicePort> {
    vec![
        ServicePort {
            name: Some("flight".into()),
            port: 50051,
            protocol: Some("TCP".into()),
            target_port: Some(IntOrString::String("flight".into())),
            ..Default::default()
        },
        ServicePort {
            name: Some("control".into()),
            port: 50052,
            protocol: Some("TCP".into()),
            target_port: Some(IntOrString::String("control".into())),
            ..Default::default()
        },
        ServicePort {
            name: Some("metrics".into()),
            port: 9750,
            protocol: Some("TCP".into()),
            target_port: Some(IntOrString::String("metrics".into())),
            ..Default::default()
        },
    ]
}

fn role_service(cluster: &YesnoCluster, suffix: &str, role: InstanceRole) -> Service {
    let mut selector = labels(cluster);
    selector.insert(ROLE_LABEL.into(), role.as_str().into());
    Service {
        metadata: ObjectMeta {
            name: Some(format!("{}-{suffix}", base_name(cluster))),
            namespace: cluster.namespace(),
            labels: Some(labels(cluster)),
            owner_references: Some(vec![owner(cluster)]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            ports: Some(service_ports()),
            selector: Some(selector),
            type_: Some("ClusterIP".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn read_write_service(cluster: &YesnoCluster) -> Service {
    role_service(cluster, "rw", InstanceRole::Leader)
}

pub(crate) fn read_only_service(cluster: &YesnoCluster) -> Service {
    role_service(cluster, "ro", InstanceRole::Follower)
}

pub(crate) fn instance_service(cluster: &YesnoCluster, instance: i32) -> Service {
    Service {
        metadata: ObjectMeta {
            name: Some(instance_name(cluster, instance)),
            namespace: cluster.namespace(),
            labels: Some(instance_labels(cluster, instance)),
            owner_references: Some(vec![owner(cluster)]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            ports: Some(service_ports()),
            selector: Some(instance_labels(cluster, instance)),
            publish_not_ready_addresses: Some(true),
            type_: Some("ClusterIP".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn deployment(
    cluster: &YesnoCluster,
    instance: i32,
    role: InstanceRole,
    source: &ConfigSource,
    replicas: i32,
) -> Deployment {
    let name = instance_name(cluster, instance);
    let selector_labels = instance_labels(cluster, instance);
    let mut pod_labels = cluster.spec.pod_labels.clone();
    pod_labels.extend(role_labels(cluster, instance, role));

    let (config_volume, mut config_identity, insecure, tls_mount) = match source {
        ConfigSource::Generated { name, body, tls } => (
            Volume {
                name: "config".into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: name.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            format!("configmap:{name}:{:08x}", crc32c::crc32c(body.as_bytes())),
            tls.is_none(),
            tls.as_ref(),
        ),
        ConfigSource::Secret { name, identity } => (
            Volume {
                name: "config".into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(name.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            format!("secret:{identity}"),
            false,
            None,
        ),
    };
    if let Some(tls) = tls_mount {
        config_identity.push_str(&format!(":tls:{}", tls.identity));
    }

    let mut args = vec!["--config".into(), CONFIG_PATH.into()];
    if insecure {
        args.push("--insecure".into());
        args.push("--insecure-replication".into());
    }

    let probe = |path: &str| Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::String("metrics".into()),
            scheme: Some("HTTP".into()),
            ..Default::default()
        }),
        period_seconds: Some(10),
        timeout_seconds: Some(3),
        failure_threshold: Some(3),
        ..Default::default()
    };

    Deployment {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: cluster.namespace(),
            labels: Some(role_labels(cluster, instance, role)),
            owner_references: Some(vec![owner(cluster)]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: LabelSelector {
                // Role is intentionally excluded: a promotion updates a label
                // without violating Deployment selector immutability.
                match_labels: Some(selector_labels),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".into()),
                ..Default::default()
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    annotations: Some(BTreeMap::from([(
                        "yesnodb.io/config-identity".into(),
                        config_identity,
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    automount_service_account_token: Some(false),
                    // Deliberately together with the line above. The daemon
                    // never talks to the Kubernetes API, so it mounts no API
                    // token; a ServiceAccount is named only because a cloud
                    // provider hangs an identity off one -- on EKS the IRSA
                    // webhook projects its token into a volume of its own,
                    // which `automountServiceAccountToken` does not govern.
                    service_account_name: cluster.spec.service_account_name.clone(),
                    containers: vec![Container {
                        name: "yesnod".into(),
                        image: Some(cluster.spec.image.clone()),
                        image_pull_policy: Some(
                            cluster
                                .spec
                                .image_pull_policy
                                .clone()
                                .unwrap_or_else(|| "IfNotPresent".into()),
                        ),
                        args: Some(args),
                        ports: Some(vec![
                            ContainerPort {
                                container_port: 50051,
                                name: Some("flight".into()),
                                protocol: Some("TCP".into()),
                                ..Default::default()
                            },
                            ContainerPort {
                                container_port: 50052,
                                name: Some("control".into()),
                                protocol: Some("TCP".into()),
                                ..Default::default()
                            },
                            ContainerPort {
                                container_port: 9750,
                                name: Some("metrics".into()),
                                protocol: Some("TCP".into()),
                                ..Default::default()
                            },
                        ]),
                        resources: cluster.spec.resources.clone(),
                        readiness_probe: Some(probe("/readyz")),
                        liveness_probe: Some(probe("/healthz")),
                        security_context: Some(SecurityContext {
                            allow_privilege_escalation: Some(false),
                            capabilities: Some(Capabilities {
                                drop: Some(vec!["ALL".into()]),
                                ..Default::default()
                            }),
                            read_only_root_filesystem: Some(true),
                            run_as_group: Some(10001),
                            run_as_non_root: Some(true),
                            run_as_user: Some(10001),
                            ..Default::default()
                        }),
                        volume_mounts: Some({
                            let mut mounts = vec![
                                VolumeMount {
                                    name: "config".into(),
                                    mount_path: "/etc/yesno".into(),
                                    read_only: Some(true),
                                    ..Default::default()
                                },
                                VolumeMount {
                                    name: "data".into(),
                                    mount_path: DATA_PATH.into(),
                                    ..Default::default()
                                },
                            ];
                            if tls_mount.is_some() {
                                mounts.extend([
                                    VolumeMount {
                                        name: "tls".into(),
                                        mount_path: TLS_PATH.into(),
                                        read_only: Some(true),
                                        ..Default::default()
                                    },
                                    VolumeMount {
                                        name: "ca".into(),
                                        mount_path: "/etc/yesno/ca".into(),
                                        read_only: Some(true),
                                        ..Default::default()
                                    },
                                ]);
                            }
                            mounts
                        }),
                        ..Default::default()
                    }],
                    enable_service_links: Some(false),
                    node_selector: (!cluster.spec.node_selector.is_empty())
                        .then(|| cluster.spec.node_selector.clone()),
                    security_context: Some(k8s_openapi::api::core::v1::PodSecurityContext {
                        fs_group: Some(10001),
                        fs_group_change_policy: Some("OnRootMismatch".into()),
                        run_as_group: Some(10001),
                        run_as_non_root: Some(true),
                        run_as_user: Some(10001),
                        seccomp_profile: Some(k8s_openapi::api::core::v1::SeccompProfile {
                            type_: "RuntimeDefault".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    termination_grace_period_seconds: Some(
                        i64::try_from(cluster.spec.shutdown_grace_secs)
                            .expect("validated shutdown grace fits i64")
                            + 10,
                    ),
                    volumes: Some({
                        let mut volumes = vec![
                            config_volume,
                            Volume {
                                name: "data".into(),
                                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                    claim_name: data_claim_name(cluster, instance),
                                    read_only: Some(false),
                                }),
                                ..Default::default()
                            },
                        ];
                        if let Some(tls) = tls_mount {
                            volumes.extend([
                                Volume {
                                    name: "tls".into(),
                                    secret: Some(SecretVolumeSource {
                                        secret_name: Some(tls.server_secret.clone()),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                                Volume {
                                    name: "ca".into(),
                                    secret: Some(SecretVolumeSource {
                                        secret_name: Some(tls.ca_secret.clone()),
                                        items: Some(vec![k8s_openapi::api::core::v1::KeyToPath {
                                            key: tls.ca_key.clone(),
                                            path: "ca.crt".into(),
                                            ..Default::default()
                                        }]),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                            ]);
                        }
                        volumes
                    }),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::api::{ConfigSpec, SnapshotSpec, StorageSpec, YesnoClusterSpec};

    fn cluster() -> YesnoCluster {
        let mut cluster = YesnoCluster::new(
            "search",
            YesnoClusterSpec {
                image: "ghcr.io/example/yesnod:v1".into(),
                image_pull_policy: None,
                storage: StorageSpec {
                    size: Some("10Gi".into()),
                    ..Default::default()
                },
                instances: 2,
                failover_delay_secs: 30,
                shards: 4,
                config: ConfigSpec {
                    allow_insecure: true,
                    ..Default::default()
                },
                snapshot: SnapshotSpec::default(),
                service_account_name: None,
                resources: None,
                pod_labels: BTreeMap::new(),
                node_selector: BTreeMap::new(),
                shutdown_grace_secs: 30,
            },
        );
        cluster.metadata.namespace = Some("data".into());
        cluster.metadata.uid = Some("test-uid".into());
        cluster
    }

    #[test]
    fn topology_uses_non_overlapping_recreate_deployments() {
        let cluster = cluster();
        let source = config_source(&cluster, 0, InstanceRole::Leader, None, None, None);
        let deployment = deployment(&cluster, 0, InstanceRole::Leader, &source, 1);
        let spec = deployment.spec.unwrap();

        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.strategy.unwrap().type_.as_deref(), Some("Recreate"));
        assert_eq!(
            spec.selector.match_labels.unwrap().get(INSTANCE_LABEL),
            Some(&"0".to_string())
        );
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.termination_grace_period_seconds, Some(40));
        assert_eq!(
            pod.containers[0].args.as_deref(),
            Some(
                &[
                    "--config".to_string(),
                    CONFIG_PATH.to_string(),
                    "--insecure".to_string(),
                    "--insecure-replication".to_string(),
                ][..]
            )
        );
    }

    #[test]
    fn generated_leader_and_follower_configs_validate() {
        for (instance, role) in [(0, InstanceRole::Leader), (1, InstanceRole::Follower)] {
            let ConfigSource::Generated { body, .. } =
                config_source(&cluster(), instance, role, None, None, None)
            else {
                panic!("expected generated configuration")
            };
            assert!(body.contains("listen = \"0.0.0.0:50052\""));
            assert!(body.contains("capability = \"replication\""));
            if role == InstanceRole::Follower {
                assert!(body.contains("leader = \"http://search-rw:50052\""));
            }
            let parsed: yesno_server::config::Config = toml::from_str(&body).unwrap();
            parsed.validate_with(true, true).unwrap();
        }
    }

    #[test]
    fn cert_manager_topology_is_mutual_tls_and_validates() {
        let mut cluster = cluster();
        cluster.spec.config.allow_insecure = false;
        cluster.spec.config.cert_manager = Some(crate::api::CertManagerSpec {
            issuer_ref: crate::api::IssuerReference {
                name: "yesno-ca".into(),
                kind: "Issuer".into(),
                group: "cert-manager.io".into(),
            },
            ca_secret_ref: crate::api::SecretKeyReference {
                name: "yesno-root-ca".into(),
                key: "tls.crt".into(),
            },
        });
        let tls = TlsTopology {
            client_fingerprint: "11".repeat(32),
            instance_fingerprints: vec!["22".repeat(32), "33".repeat(32)],
            ca_secret: "yesno-root-ca".into(),
            ca_key: "tls.crt".into(),
            identity: "revision".into(),
        };
        for (instance, role) in [(0, InstanceRole::Leader), (1, InstanceRole::Follower)] {
            let source = config_source(&cluster, instance, role, None, Some(&tls), None);
            let ConfigSource::Generated {
                body,
                tls: Some(mount),
                ..
            } = &source
            else {
                panic!("expected generated TLS configuration")
            };
            assert!(body.contains("require_client_auth = true"));
            assert!(body.contains("channel = \"hostssl\""));
            assert!(!body.contains("principal = \"all\""));
            assert_eq!(mount.server_secret, format!("search-{instance}-tls"));
            if role == InstanceRole::Follower {
                assert!(body.contains("leader = \"https://search-rw:50052\""));
                assert!(body.contains("domain = \"search-rw\""));
            }
            let parsed: yesno_server::config::Config = toml::from_str(body).unwrap();
            parsed.validate(false).unwrap();

            let pod = deployment(&cluster, instance, role, &source, 1)
                .spec
                .unwrap()
                .template
                .spec
                .unwrap();
            assert_eq!(pod.volumes.unwrap().len(), 4);
            assert_eq!(pod.containers[0].volume_mounts.as_ref().unwrap().len(), 4);
            assert_eq!(
                pod.containers[0].args.as_deref(),
                Some(&["--config".to_string(), CONFIG_PATH.to_string()][..])
            );
        }

        let cert = instance_certificate(&cluster, 0);
        assert_eq!(cert.data["spec"]["secretName"], "search-0-tls");
        assert_eq!(cert.data["spec"]["privateKey"]["rotationPolicy"], "Always");
        assert!(cert.data["spec"]["dnsNames"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "search-rw.data.svc"));
    }

    fn ebs_cluster() -> YesnoCluster {
        let mut cluster = cluster();
        cluster.spec.snapshot = SnapshotSpec {
            backend: SnapshotBackend::Ebs,
            ebs: Some(crate::api::EbsSnapshotSpec {
                region: "ap-northeast-1".into(),
                filesystem: "ext4".into(),
                operation_timeout_secs: 900,
                resource_tags: BTreeMap::new(),
            }),
            lease_ttl_secs: 600,
        };
        cluster
    }

    fn generated(cluster: &YesnoCluster, volume_id: Option<&str>) -> String {
        let ConfigSource::Generated { body, .. } =
            config_source(cluster, 0, InstanceRole::Leader, None, None, volume_id)
        else {
            panic!("expected generated configuration")
        };
        body
    }

    #[test]
    fn a_bound_volume_configures_deferred_ebs_snapshots() {
        let body = generated(&ebs_cluster(), Some("vol-0123456789abcdef0"));
        let parsed: yesno_server::config::Config = toml::from_str(&body).unwrap();
        parsed.validate_with(true, true).unwrap();

        let snapshot = &parsed.server.snapshot;
        assert_eq!(snapshot.backend, yesno_server::config::SnapshotBackend::Ebs);
        assert_eq!(snapshot.lease_ttl_secs, 600);
        let ebs = snapshot.ebs.as_ref().expect("EBS provider settings");
        // Deferred is not selectable, and the reason it must not become
        // selectable is the Pod: local materialization mounts, and this Pod
        // drops every capability.
        assert_eq!(
            ebs.materialization,
            yesno_server::config::EbsMaterialization::Deferred
        );
        assert_eq!(ebs.volume_id, "vol-0123456789abcdef0");
        assert_eq!(ebs.region, "ap-northeast-1");
        assert_eq!(ebs.filesystem, "ext4");
        assert_eq!(ebs.operation_timeout_secs, 900);
        // The claim is mounted at the data directory, so the database is the
        // root of the volume the snapshot is taken from.
        assert_eq!(ebs.source_mount, PathBuf::from(DATA_PATH));
        assert_eq!(parsed.data_dir(), Path::new(DATA_PATH));
        // Local-only fields stay unset; the server rejects a partition here.
        assert!(ebs.instance_id.is_empty());
        assert!(ebs.availability_zone.is_empty());
        assert_eq!(ebs.partition, None);
    }

    #[test]
    fn a_bound_volume_configures_ebs_under_mutual_tls_too() {
        let mut cluster = ebs_cluster();
        cluster.spec.config.allow_insecure = false;
        let tls = TlsTopology {
            client_fingerprint: "11".repeat(32),
            instance_fingerprints: vec!["22".repeat(32), "33".repeat(32)],
            ca_secret: "yesno-root-ca".into(),
            ca_key: "tls.crt".into(),
            identity: "revision".into(),
        };
        let ConfigSource::Generated { body, .. } = config_source(
            &cluster,
            1,
            InstanceRole::Follower,
            None,
            Some(&tls),
            Some("vol-00ff"),
        ) else {
            panic!("expected generated configuration")
        };
        let parsed: yesno_server::config::Config = toml::from_str(&body).unwrap();
        parsed.validate_with(false, false).unwrap();
        assert_eq!(
            parsed.server.snapshot.ebs.as_ref().unwrap().volume_id,
            "vol-00ff"
        );
        // The snapshot section must not have displaced the topology it was
        // inserted in front of.
        assert!(body.contains("leader = \"https://search-rw:50052\""));
        assert!(body.contains("require_client_auth = true"));
    }

    #[test]
    fn an_unbound_claim_leaves_the_portable_provider() {
        let body = generated(&ebs_cluster(), None);
        assert!(!body.contains("[server.snapshot]"), "{body}");
        let parsed: yesno_server::config::Config = toml::from_str(&body).unwrap();
        parsed.validate_with(true, true).unwrap();
        // Disabled here means the portable staged copy, not "no snapshots".
        // A cluster in this window still serves a lease; it just does not use
        // EBS yet. See `snapshot_section`.
        assert_eq!(
            parsed.server.snapshot.backend,
            yesno_server::config::SnapshotBackend::Disabled
        );
        assert!(parsed.server.snapshot.ebs.is_none());
    }

    #[test]
    fn a_disabled_backend_generates_nothing_even_with_settings_present() {
        let mut cluster = ebs_cluster();
        cluster.spec.snapshot.backend = SnapshotBackend::Disabled;
        let body = generated(&cluster, Some("vol-00ff"));
        assert!(!body.contains("snapshot"), "{body}");
    }

    #[test]
    fn resource_tags_survive_the_round_trip_through_toml() {
        let mut cluster = ebs_cluster();
        cluster.spec.snapshot.ebs.as_mut().unwrap().resource_tags = BTreeMap::from([
            ("cost-center".into(), "search".into()),
            ("kubernetes.io/cluster".into(), "prod/eu".into()),
        ]);
        let body = generated(&cluster, Some("vol-00ff"));
        let parsed: yesno_server::config::Config = toml::from_str(&body).unwrap();
        parsed.validate_with(true, true).unwrap();
        let tags = &parsed.server.snapshot.ebs.as_ref().unwrap().resource_tags;
        assert_eq!(tags.get("cost-center").map(String::as_str), Some("search"));
        assert_eq!(
            tags.get("kubernetes.io/cluster").map(String::as_str),
            Some("prod/eu")
        );
    }

    /// A tag is namespace-user input and the output is a file the daemon
    /// obeys, so a value must not be able to close its string and open a table.
    #[test]
    fn a_tag_cannot_inject_a_configuration_table() {
        let mut cluster = ebs_cluster();
        let hostile = "x\"\n[server.flight]\nlisten = \"0.0.0.0:1\"\n#";
        cluster.spec.snapshot.ebs.as_mut().unwrap().resource_tags =
            BTreeMap::from([("owner".into(), hostile.into())]);
        let body = generated(&cluster, Some("vol-00ff"));
        let parsed: yesno_server::config::Config = toml::from_str(&body).unwrap();
        assert_eq!(
            parsed.server.snapshot.ebs.as_ref().unwrap().resource_tags["owner"],
            hostile
        );
        assert_eq!(parsed.server.flight.listen, "0.0.0.0:50051");
    }

    #[test]
    fn learning_the_volume_changes_the_pod_template_identity() {
        let cluster = ebs_cluster();
        let identity = |volume_id| {
            let source = config_source(&cluster, 0, InstanceRole::Leader, None, None, volume_id);
            deployment(&cluster, 0, InstanceRole::Leader, &source, 1)
                .spec
                .unwrap()
                .template
                .metadata
                .unwrap()
                .annotations
                .unwrap()["yesnodb.io/config-identity"]
                .clone()
        };
        // The Pod must restart when the backend appears, which is the whole
        // cost of resolving the volume after the claim binds rather than
        // before.
        assert_ne!(identity(None), identity(Some("vol-00ff")));
        assert_ne!(identity(Some("vol-00ff")), identity(Some("vol-00fe")));
    }

    #[test]
    fn a_service_account_reaches_the_pod_without_an_api_token() {
        let mut cluster = ebs_cluster();
        cluster.spec.service_account_name = Some("yesno-snapshotter".into());
        let source = config_source(&cluster, 0, InstanceRole::Leader, None, None, None);
        let pod = deployment(&cluster, 0, InstanceRole::Leader, &source, 1)
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        assert_eq!(
            pod.service_account_name.as_deref(),
            Some("yesno-snapshotter")
        );
        // The two belong together: the identity is for AWS, not for the
        // Kubernetes API, and the projected IRSA token is a separate volume.
        assert_eq!(pod.automount_service_account_token, Some(false));
    }

    #[test]
    fn role_services_do_not_overlap() {
        let cluster = cluster();
        let rw = read_write_service(&cluster).spec.unwrap().selector.unwrap();
        let ro = read_only_service(&cluster).spec.unwrap().selector.unwrap();
        assert_eq!(rw.get(ROLE_LABEL).map(String::as_str), Some("leader"));
        assert_eq!(ro.get(ROLE_LABEL).map(String::as_str), Some("follower"));
        assert_ne!(rw, ro);
        assert_eq!(
            instance_service(&cluster, 1)
                .spec
                .unwrap()
                .publish_not_ready_addresses,
            Some(true)
        );
    }

    #[test]
    fn every_managed_instance_has_an_independent_retained_claim() {
        let cluster = cluster();
        let first = persistent_volume_claim(&cluster, 0);
        let second = persistent_volume_claim(&cluster, 1);
        assert_eq!(first.metadata.name.as_deref(), Some("search-0-data"));
        assert_eq!(second.metadata.name.as_deref(), Some("search-1-data"));
        assert!(first.metadata.owner_references.is_none());
        assert!(second.metadata.owner_references.is_none());
        assert_eq!(
            first.spec.unwrap().volume_mode.as_deref(),
            Some("Filesystem")
        );
    }

    #[test]
    fn secret_revision_changes_the_pod_template_identity() {
        let cluster = cluster();
        let first = deployment(
            &cluster,
            0,
            InstanceRole::Leader,
            &ConfigSource::Secret {
                name: "config".into(),
                identity: "config:aaaa".into(),
            },
            1,
        );
        let second = deployment(
            &cluster,
            0,
            InstanceRole::Leader,
            &ConfigSource::Secret {
                name: "config".into(),
                identity: "config:bbbb".into(),
            },
            1,
        );
        assert_ne!(first.spec.unwrap().template, second.spec.unwrap().template);
    }

    #[test]
    fn long_and_dotted_names_become_stable_dns_labels() {
        let mut c = cluster();
        c.metadata.name = Some(
            "this.is.a.very.long.cluster.name.that.would.not.fit.with.resource.suffixes".into(),
        );
        let a = base_name(&c);
        let b = base_name(&c);
        assert_eq!(a, b);
        assert!(a.len() <= 48);
        assert!(!a.contains('.'));
    }
}
