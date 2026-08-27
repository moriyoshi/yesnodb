//! Best-effort cleanup for a Terraform-owned AWS E2E run.
//!
//! The behavioral scenario proves normal cleanup. This helper is the failure
//! path: the EC2 runner invokes it from a shell trap so an assertion failure
//! does not leave chargeable snapshot clones behind.

use std::process::ExitCode;
use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_sdk_ec2::client::Waiters as _;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::types::VolumeState;
use aws_sdk_ec2::Client;
use yesno_e2e::aws::provider_resource_filters;

#[tokio::main]
async fn main() -> ExitCode {
    match cleanup().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("AWS E2E cleanup failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn cleanup() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let region = required_env("YESNO_AWS_REGION")?;
    let run_id = required_env("YESNO_AWS_E2E_RUN_ID")?;
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;
    let client = Client::new(&shared);
    let filters = provider_resource_filters(&run_id);

    let volumes = client
        .describe_volumes()
        .set_filters(Some(filters.clone()))
        .send()
        .await?
        .volumes
        .unwrap_or_default();
    for volume in volumes {
        let Some(volume_id) = volume.volume_id() else {
            continue;
        };
        if volume.state() == Some(&VolumeState::InUse) {
            for attachment in volume.attachments() {
                client
                    .detach_volume()
                    .volume_id(volume_id)
                    .set_instance_id(attachment.instance_id().map(ToOwned::to_owned))
                    .send()
                    .await?;
            }
            client
                .wait_until_volume_available()
                .volume_ids(volume_id)
                .wait(Duration::from_secs(300))
                .await?;
        }
        client.delete_volume().volume_id(volume_id).send().await?;
    }

    let snapshots = client
        .describe_snapshots()
        .owner_ids("self")
        .set_filters(Some(filters))
        .send()
        .await?
        .snapshots
        .unwrap_or_default();
    for snapshot in snapshots {
        if let Some(snapshot_id) = snapshot.snapshot_id() {
            client
                .delete_snapshot()
                .snapshot_id(snapshot_id)
                .send()
                .await?;
        }
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("required environment variable {name} is not set").into())
}
