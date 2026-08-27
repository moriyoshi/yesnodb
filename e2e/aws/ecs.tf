# Resources for the deferred-materialization half of the gate.
#
# `e2e/aws/ebs.py` proves local materialization: yesnod restores its own
# snapshot onto the runner. This file provisions the other arm of the same
# decision -- `materialization = "deferred"`, where yesnod hands the archiver a
# provisional snapshot and never mounts anything, and the archiver launches a
# Fargate task that restores it, stages the file set into storage both
# processes can see, and exits.
#
# Everything here hangs off the VPC, subnet, image, and instance role in
# main.tf on purpose: the two scenarios run on the same runner, from one
# `apply`, so proving the second arm costs a task and a filesystem rather than
# a second stack.

locals {
  # One path, used three times: the container mount in the task definition
  # below, the NFS mount the runner makes, and the archiver's
  # `--materializer-staging-path`. The archiver hands the worker an absolute
  # target and then reads it back itself, so the two machines must agree on it
  # exactly. It is an output for that reason.
  staging_mount = "/mnt/yesno-staging"
  # Where the restored EBS volume appears inside the task. The archiver joins
  # the lease's own subpath onto this, so a wrong value fails the worker rather
  # than silently staging nothing -- which is what the failure half of
  # `deferred_ecs.py` deliberately does.
  task_source_path  = "/mnt/yesno-ebs"
  stage_container   = "stage"
  stage_volume      = "source"
  materializer_name = "yesno-e2e-${var.run_id}"
}

# Shared staging. EFS rather than a second EBS volume because the archiver and
# the worker are on different machines and both write to it; a Fargate task
# cannot attach a volume the EC2 runner already has.
resource "aws_efs_file_system" "staging" {
  encrypted = true

  tags = {
    Name = local.materializer_name
  }
}

resource "aws_security_group" "staging" {
  name_prefix = "yesno-e2e-efs-"
  description = "NFS from the runner and from the materializer task"
  vpc_id      = aws_vpc.this.id
}

resource "aws_vpc_security_group_ingress_rule" "staging_from_runner" {
  security_group_id            = aws_security_group.staging.id
  referenced_security_group_id = aws_security_group.runner.id
  from_port                    = 2049
  to_port                      = 2049
  ip_protocol                  = "tcp"
}

resource "aws_vpc_security_group_ingress_rule" "staging_from_materializer" {
  security_group_id            = aws_security_group.staging.id
  referenced_security_group_id = aws_security_group.materializer.id
  from_port                    = 2049
  to_port                      = 2049
  ip_protocol                  = "tcp"
}

resource "aws_efs_mount_target" "staging" {
  file_system_id  = aws_efs_file_system.staging.id
  subnet_id       = aws_subnet.runner.id
  security_groups = [aws_security_group.staging.id]
}

# The task's own group. No ingress: it pulls an image, mounts EFS, and reaches
# the ECS control plane, all outbound.
resource "aws_security_group" "materializer" {
  name_prefix = "yesno-e2e-task-"
  description = "No ingress; the Fargate materializer only makes outbound connections"
  vpc_id      = aws_vpc.this.id
}

resource "aws_vpc_security_group_egress_rule" "materializer" {
  security_group_id = aws_security_group.materializer.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

resource "aws_ecs_cluster" "materializer" {
  name = local.materializer_name
}

# One day is the minimum CloudWatch offers and this log group outlives the run
# only until `destroy`. It exists because a worker that fails inside the task
# has nowhere else to say why: the archiver reports the container's exit code,
# not its output.
resource "aws_cloudwatch_log_group" "materializer" {
  name              = "/yesno-e2e/${var.run_id}"
  retention_in_days = 1
}

data "aws_iam_policy_document" "ecs_tasks_assume" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "task_execution" {
  name_prefix        = "yesno-e2e-exec-"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

resource "aws_iam_role_policy_attachment" "task_execution" {
  role       = aws_iam_role.task_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role" "task" {
  name_prefix        = "yesno-e2e-task-"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

data "aws_iam_policy_document" "task" {
  statement {
    sid = "MountStaging"
    actions = [
      "elasticfilesystem:ClientMount",
      "elasticfilesystem:ClientRootAccess",
      "elasticfilesystem:ClientWrite",
    ]
    resources = [aws_efs_file_system.staging.arn]
  }
}

resource "aws_iam_role_policy" "task" {
  name_prefix = "yesno-e2e-task-"
  role        = aws_iam_role.task.id
  policy      = data.aws_iam_policy_document.task.json
}

# The role ECS itself assumes to create, attach and delete the volume it
# restores from the provisional snapshot. It is passed to `RunTask` by the
# archiver and is the reason the archiver needs no EC2 permission of its own.
data "aws_iam_policy_document" "ecs_infrastructure_assume" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ecs.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "ecs_infrastructure" {
  name_prefix        = "yesno-e2e-infra-"
  assume_role_policy = data.aws_iam_policy_document.ecs_infrastructure_assume.json
}

resource "aws_iam_role_policy_attachment" "ecs_infrastructure" {
  role       = aws_iam_role.ecs_infrastructure.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSInfrastructureRolePolicyForVolumes"
}

# What the runner -- and therefore the archiver running on it -- may do to ECS.
# Scoped to this run's cluster and task-definition family, so the gate cannot
# launch or stop anything belonging to another run in the same account.
data "aws_iam_policy_document" "runner_ecs" {
  statement {
    sid       = "LaunchMaterializer"
    actions   = ["ecs:RunTask"]
    resources = ["arn:aws:ecs:${var.region}:${data.aws_caller_identity.current.account_id}:task-definition/${local.materializer_name}:*"]

    condition {
      test     = "ArnEquals"
      variable = "ecs:cluster"
      values   = [aws_ecs_cluster.materializer.arn]
    }
  }

  statement {
    sid       = "ObserveMaterializer"
    actions   = ["ecs:DescribeTasks", "ecs:StopTask"]
    resources = ["arn:aws:ecs:${var.region}:${data.aws_caller_identity.current.account_id}:task/${local.materializer_name}/*"]

    condition {
      test     = "ArnEquals"
      variable = "ecs:cluster"
      values   = [aws_ecs_cluster.materializer.arn]
    }
  }

  # Only the gate's own failure cleanup lists: if a run is interrupted between
  # launching a worker and its natural exit, `terraform destroy` cannot delete
  # a cluster with an active task, and a stack that will not destroy is the one
  # failure mode this gate must never have.
  statement {
    sid       = "ListMaterializers"
    actions   = ["ecs:ListTasks"]
    resources = ["*"]

    condition {
      test     = "ArnEquals"
      variable = "ecs:cluster"
      values   = [aws_ecs_cluster.materializer.arn]
    }
  }

  # RunTask carries the lease tags the provider puts on the restored volume, so
  # the launch is also a tagging call.
  statement {
    sid       = "TagMaterializer"
    actions   = ["ecs:TagResource"]
    resources = ["arn:aws:ecs:${var.region}:${data.aws_caller_identity.current.account_id}:task/${local.materializer_name}/*"]

    condition {
      test     = "StringEquals"
      variable = "ecs:CreateAction"
      values   = ["RunTask"]
    }
  }

  statement {
    sid     = "PassMaterializerRoles"
    actions = ["iam:PassRole"]

    resources = [
      aws_iam_role.ecs_infrastructure.arn,
      aws_iam_role.task.arn,
      aws_iam_role.task_execution.arn,
    ]
  }

  statement {
    sid = "MountStaging"
    actions = [
      "elasticfilesystem:ClientMount",
      "elasticfilesystem:ClientRootAccess",
      "elasticfilesystem:ClientWrite",
    ]
    resources = [aws_efs_file_system.staging.arn]
  }
}

resource "aws_iam_role_policy" "runner_ecs" {
  name_prefix = "yesno-e2e-ecs-"
  role        = aws_iam_role.runner.id
  policy      = data.aws_iam_policy_document.runner_ecs.json
}

# The container has no `command` and the image has no ENTRYPOINT. That is
# deliberate: the archiver supplies the whole argv as a container override
# ( `yesno-snapshot-stage --source ... --target ...` ), and an ENTRYPOINT would
# prepend the E2E harness to it. See yesno-operator/e2e/aws.Dockerfile.
resource "aws_ecs_task_definition" "materializer" {
  family                   = local.materializer_name
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = "1024"
  memory                   = "2048"
  execution_role_arn       = aws_iam_role.task_execution.arn
  task_role_arn            = aws_iam_role.task.arn

  runtime_platform {
    operating_system_family = "LINUX"
    cpu_architecture        = var.architecture == "arm64" ? "ARM64" : "X86_64"
  }

  # Restored from the provisional snapshot at launch, which is what
  # `configure_at_launch` means: the snapshot id is not known when this is
  # registered, only when the archiver holds a lease.
  volume {
    name                = local.stage_volume
    configure_at_launch = true
  }

  volume {
    name = "staging"

    efs_volume_configuration {
      file_system_id     = aws_efs_file_system.staging.id
      transit_encryption = "ENABLED"
      root_directory     = "/"
    }
  }

  container_definitions = jsonencode([
    {
      name      = local.stage_container
      image     = local.image_uri
      essential = true
      mountPoints = [
        # Read-only, which is the documented shape of the deferred branch:
        # the worker has a read-only restored source and writes only the
        # bounded file set to shared staging. ECS mounts the filesystem itself
        # at task level, so journal recovery on a crash-consistent snapshot is
        # not this flag's problem.
        {
          sourceVolume  = local.stage_volume
          containerPath = local.task_source_path
          readOnly      = true
        },
        {
          sourceVolume  = "staging"
          containerPath = local.staging_mount
          readOnly      = false
        },
      ]
      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.materializer.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "stage"
        }
      }
    },
  ])
}
