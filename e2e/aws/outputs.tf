# Terraform's whole interface to the orchestration. Everything the harness
# needs to publish the image, address the runner, and compose the runner's
# environment is read back through `terraform output -raw`, so a resource that
# is renamed here breaks the gate at `cloud_output()` with the missing output
# named, rather than several minutes later on the instance.

output "run_id" {
  value = var.run_id
}

output "runner_instance_id" {
  value = aws_instance.runner.id
}

output "source_volume_id" {
  value = aws_ebs_volume.source.id
}

output "availability_zone" {
  value = local.availability_zone
}

output "driver_image" {
  value = local.image_uri
}

# The operator arm's two images, which differ from `driver_image` only in
# having an ENTRYPOINT. A `YesnoCluster` names an image and supplies `args`
# alone, so the image has to start its own process; the deferred arms need the
# opposite and use `driver_image`.
output "yesnod_image" {
  value = local.yesnod_image_uri
}

output "operator_image" {
  value = local.operator_image_uri
}

output "driver_platform" {
  value = local.platform
}

# The deferred-materialization half. `cloud_ecs_env()` reads exactly these and
# names them after the environment variables the runner requires, so the two
# lists cannot drift apart without a unit test failing.

output "ecs_cluster" {
  value = aws_ecs_cluster.materializer.name
}

output "ecs_task_definition" {
  value = aws_ecs_task_definition.materializer.arn
}

output "ecs_container_name" {
  value = local.stage_container
}

output "ecs_volume_name" {
  value = local.stage_volume
}

output "ecs_infrastructure_role_arn" {
  value = aws_iam_role.ecs_infrastructure.arn
}

output "ecs_subnets" {
  value = aws_subnet.runner.id
}

output "ecs_security_groups" {
  value = aws_security_group.materializer.id
}

output "efs_file_system_id" {
  value = aws_efs_file_system.staging.id
}

# The same absolute path on the runner and inside the task. See ecs.tf.
output "staging_mount" {
  value = local.staging_mount
}

output "task_source_path" {
  value = local.task_source_path
}

# The EKS arm. `eks.tf` decides these names; the runner's bootstrap creates
# them in the cluster and the archiver is then told to use them, so nothing is
# spelled twice.
#
# The three that come from the cluster use `one()`, which is null rather than
# an index error when `var.eks` is false. The rest are constants and stay as
# they are — a name the stack did not build is still the name it would have
# used, and `gate.py` reads none of these unless the arm is on.

output "eks_cluster_name" {
  value = one(aws_eks_cluster.this[*].name)
}

output "eks_endpoint" {
  value = one(aws_eks_cluster.this[*].endpoint)
}

output "eks_certificate_authority" {
  value = one(aws_eks_cluster.this[*].certificate_authority[0].data)
}

output "eks_namespace" {
  value = local.eks_namespace
}

output "eks_archive_account" {
  value = local.eks_archive_account
}

output "eks_snapshot_class" {
  value = local.eks_snapshot_class
}

output "eks_storage_class" {
  value = local.eks_storage_class
}

output "eks_staging_claim" {
  value = local.eks_staging_claim
}

# The absolute path of the kubeconfig on the runner, used both by the script
# that writes it and by the container that reads it.
output "kubeconfig_path" {
  value = local.kubeconfig_path
}

# The operator arm. The namespace and account are also named in the IRSA
# trust policy, so exactly one file decides them and the token the webhook
# projects is the one the role trusts.
output "eks_operator_namespace" {
  value = local.eks_operator_namespace
}

output "eks_operator_account" {
  value = local.eks_operator_account
}

output "eks_operator_role_arn" {
  value = one(aws_iam_role.eks_yesnod[*].arn)
}

output "operator_kubeconfig_path" {
  value = local.operator_kubeconfig_path
}
