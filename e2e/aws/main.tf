data "aws_caller_identity" "current" {}
data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  availability_zone = data.aws_availability_zones.available.names[0]
  # Burstable, because nothing here is sustained CPU: the image is built on the
  # host and pulled, and the runner then serves a small database while it waits
  # on AWS. There is no t4 family on x86-64, so that side is the equivalent
  # t3 rather than a t4 that does not exist. Both are Nitro, which the source
  # volume's NVMe-serial device resolution depends on, and both are 2 vCPU /
  # 8 GiB — the same shape the m-family defaults had.
  instance_type = var.instance_type != "" ? var.instance_type : (
    var.architecture == "arm64" ? "t4g.large" : "t3.large"
  )
  ami_parameter = var.architecture == "arm64" ? (
    "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64"
  ) : "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
  platform  = var.architecture == "arm64" ? "linux/arm64" : "linux/amd64"
  image_uri = "${aws_ecr_repository.driver.repository_url}:${var.run_id}"

  # The same repository, two more tags. Separate tags rather than separate
  # repositories, and separate images rather than one: `image_tag_mutability`
  # is IMMUTABLE, and the three differ only by ENTRYPOINT, so every layer but
  # the last is shared and already pushed by the time these are.
  yesnod_image_uri   = "${aws_ecr_repository.driver.repository_url}:${var.run_id}-yesnod"
  operator_image_uri = "${aws_ecr_repository.driver.repository_url}:${var.run_id}-operator"

  # The three EC2 resource types this stack's IAM policies name.
  #
  # A snapshot ARN has an **empty account field**, unlike an instance or a
  # volume. That is AWS's own resource model -- snapshot and image ARNs are
  # written `arn:aws:ec2:<region>::<type>/<id>` -- and writing the account into
  # one produces an ARN that matches nothing. The statement is then simply
  # absent, and the request is denied for a reason that reads as a missing
  # action rather than a malformed resource:
  #
  #   is not authorized to perform: ec2:CreateSnapshot on resource:
  #   arn:aws:ec2:ap-northeast-1::snapshot/* because no identity-based policy
  #   allows the ec2:CreateSnapshot action
  #
  # That is a live gate failure from 2026-09-02, and the ARN in the message is
  # the one AWS was looking for.
  #
  # It is not a widening. Ownership is not expressible here, but it does not
  # have to be: EC2 refuses to delete a snapshot the caller does not own
  # whatever IAM says, and the statements below are scoped by the run tag.
  ec2_instance_arn = "arn:aws:ec2:${var.region}:${data.aws_caller_identity.current.account_id}:instance/*"
  ec2_volume_arn   = "arn:aws:ec2:${var.region}:${data.aws_caller_identity.current.account_id}:volume/*"
  ec2_snapshot_arn = "arn:aws:ec2:${var.region}::snapshot/*"
}

data "aws_ssm_parameter" "ami" {
  name = local.ami_parameter
}

resource "aws_vpc" "this" {
  cidr_block           = "10.137.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
}

resource "aws_subnet" "runner" {
  vpc_id                  = aws_vpc.this.id
  availability_zone       = local.availability_zone
  cidr_block              = "10.137.1.0/24"
  map_public_ip_on_launch = true
}

resource "aws_route_table" "runner" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }
}

resource "aws_route_table_association" "runner" {
  subnet_id      = aws_subnet.runner.id
  route_table_id = aws_route_table.runner.id
}

resource "aws_security_group" "runner" {
  name_prefix = "yesno-e2e-"
  description = "No ingress; SSM reaches the runner over its outbound channel"
  vpc_id      = aws_vpc.this.id
}

resource "aws_vpc_security_group_egress_rule" "runner_ipv4" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

resource "aws_ecr_repository" "driver" {
  name                 = "yesno-e2e-${var.run_id}"
  force_delete         = true
  image_tag_mutability = "IMMUTABLE"

  encryption_configuration {
    encryption_type = "AES256"
  }

  image_scanning_configuration {
    scan_on_push = true
  }
}

data "aws_iam_policy_document" "runner_assume" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "runner" {
  name_prefix        = "yesno-e2e-"
  assume_role_policy = data.aws_iam_policy_document.runner_assume.json
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.runner.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_role_policy_attachment" "ecr" {
  role       = aws_iam_role.runner.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly"
}

data "aws_iam_policy_document" "ebs" {
  statement {
    sid = "DescribeEbs"
    actions = [
      "ec2:DescribeInstances",
      "ec2:DescribeSnapshots",
      "ec2:DescribeVolumes",
    ]
    resources = ["*"]
  }

  statement {
    sid = "CreateFromTaggedFixture"
    actions = [
      "ec2:CreateSnapshot",
      "ec2:CreateVolume",
      "ec2:CreateTags",
    ]
    resources = [
      local.ec2_instance_arn,
      local.ec2_snapshot_arn,
      local.ec2_volume_arn,
    ]
    condition {
      test     = "StringEqualsIfExists"
      variable = "aws:RequestTag/yesno:e2e-run"
      values   = [var.run_id]
    }
  }

  statement {
    sid = "OperateOnlyTaggedFixture"
    actions = [
      "ec2:AttachVolume",
      "ec2:DeleteSnapshot",
      "ec2:DeleteVolume",
      "ec2:DetachVolume",
    ]
    resources = [
      local.ec2_instance_arn,
      local.ec2_snapshot_arn,
      local.ec2_volume_arn,
    ]
    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/yesno:e2e-run"
      values   = [var.run_id]
    }
  }
}

resource "aws_iam_role_policy" "ebs" {
  name_prefix = "yesno-e2e-ebs-"
  role        = aws_iam_role.runner.id
  policy      = data.aws_iam_policy_document.ebs.json
}

resource "aws_iam_instance_profile" "runner" {
  name_prefix = "yesno-e2e-"
  role        = aws_iam_role.runner.name
}

resource "aws_instance" "runner" {
  ami                         = data.aws_ssm_parameter.ami.value
  instance_type               = local.instance_type
  availability_zone           = local.availability_zone
  subnet_id                   = aws_subnet.runner.id
  vpc_security_group_ids      = [aws_security_group.runner.id]
  associate_public_ip_address = true
  iam_instance_profile        = aws_iam_instance_profile.runner.name

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 2
  }

  root_block_device {
    encrypted   = true
    volume_size = 24
    volume_type = "gp3"
  }

  # No user data. The runner is provisioned by a `cloud_run()` script that
  # `e2e/aws/gate.py` owns, so a package or mount failure is a named scenario
  # failure with its output attached rather than a line in a cloud-init log on
  # a host with no ingress.
  depends_on = [
    aws_iam_role_policy.ebs,
    aws_iam_role_policy.runner_ecs,
    aws_iam_role_policy_attachment.ecr,
    aws_iam_role_policy_attachment.ssm,
  ]
}

resource "aws_ebs_volume" "source" {
  availability_zone = local.availability_zone
  encrypted         = true
  size              = var.source_volume_gib
  type              = "gp3"
}

resource "aws_volume_attachment" "source" {
  device_name = "/dev/sdf"
  instance_id = aws_instance.runner.id
  volume_id   = aws_ebs_volume.source.id
}
