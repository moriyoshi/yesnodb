# The second deferred materializer: an EKS Job instead of a Fargate task.
#
# `ecs.tf` proves that a provisional lease can be materialized by a workload
# the archiver launches. This file proves the *other* launcher, and the two are
# not the same test: ECS restores the snapshot with a managed volume and an
# infrastructure role, while Kubernetes restores it with a retained
# `VolumeSnapshotContent`, an EBS-CSI `PersistentVolumeClaim`, and a Job on an
# EC2 node. The archiver code paths share only the lease.
#
# This is the expensive half of the gate. A cluster and a node group take
# roughly fifteen minutes to create and ten to destroy, on top of everything
# else, which is why `YESNO_AWS_TIMEOUT` defaults where it does and why
# `var.eks` exists at all.
#
# **`var.eks` defaults to true, and a run with it off is not a run of this
# gate.** The default is on because this is the only live coverage the deferred
# EKS path has anywhere, and an arm that is off by default is an arm that rots.
# Do not flip the default to save time in CI; the knob is for a developer
# iterating on something else. `scripts/gate-aws.sh` passes it through from
# `YESNO_AWS_EKS`.

locals {
  # Names the runner's bootstrap creates in-cluster and the archiver is then
  # told to use. They are outputs so exactly one file decides them.
  eks_name            = "yesno-e2e-${var.run_id}"
  eks_namespace       = "yesno-e2e"
  eks_snapshot_class  = "yesno-ebs"
  eks_storage_class   = "yesno-ebs"
  eks_staging_claim   = "yesno-staging"
  eks_archive_account = "yesno-archive"

  # The operator arm's namespace and the ServiceAccount its yesnod Pods run
  # as. Deliberately *not* the archiver's namespace: the two arms create
  # unrelated objects, and sharing one would let a leftover from either fail
  # the other's preconditions.
  eks_operator_namespace = "yesno-operator-e2e"
  eks_operator_account   = "yesno-snapshotter"

  # Where the operator arm's admin kubeconfig is written on the runner. Beside
  # the archiver's, so the same single bind mount carries both.
  operator_kubeconfig_path = "/etc/yesno/kube/operator.config"

  # Where the runner writes the archiver's kubeconfig, and what the container
  # is given as KUBECONFIG. Written on the host so the container carries no
  # credential of its own beyond a bind mount.
  kubeconfig_path = "/etc/yesno/kube/config"
  node_ami_type   = var.architecture == "arm64" ? "AL2023_ARM_64_STANDARD" : "AL2023_x86_64_STANDARD"
}

# EKS requires subnets in at least two Availability Zones for its control-plane
# interfaces. Everything else in this gate is pinned to one zone because a
# volume and an instance in different zones cannot be attached; the control
# plane has no such constraint, and the node group stays in the first zone.
resource "aws_subnet" "runner_b" {
  count = var.eks ? 1 : 0

  vpc_id                  = aws_vpc.this.id
  availability_zone       = data.aws_availability_zones.available.names[1]
  cidr_block              = "10.137.2.0/24"
  map_public_ip_on_launch = true
}

resource "aws_route_table_association" "runner_b" {
  count = var.eks ? 1 : 0

  subnet_id      = aws_subnet.runner_b[0].id
  route_table_id = aws_route_table.runner.id
}

# The two assume-role documents below are deliberately *not* conditional.
# They read nothing from the cluster and cost no API call, so counting them
# would add two more indexes to every reference for nothing.
data "aws_iam_policy_document" "eks_cluster_assume" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["eks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "eks_cluster" {
  count = var.eks ? 1 : 0

  name_prefix        = "yesno-e2e-eks-"
  assume_role_policy = data.aws_iam_policy_document.eks_cluster_assume.json
}

resource "aws_iam_role_policy_attachment" "eks_cluster" {
  count = var.eks ? 1 : 0

  role       = aws_iam_role.eks_cluster[0].name
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKSClusterPolicy"
}

# `authentication_mode = "API"` is what lets the runner's *instance role* be
# granted cluster access as a Terraform resource. The alternative is editing
# the `aws-auth` ConfigMap, which would need a Kubernetes client before there
# is one — the bootstrap problem this gate otherwise avoids entirely.
resource "aws_eks_cluster" "this" {
  count = var.eks ? 1 : 0

  name     = local.eks_name
  role_arn = aws_iam_role.eks_cluster[0].arn

  access_config {
    authentication_mode = "API"
  }

  vpc_config {
    subnet_ids              = [aws_subnet.runner.id, aws_subnet.runner_b[0].id]
    endpoint_public_access  = true
    endpoint_private_access = true
  }

  depends_on = [aws_iam_role_policy_attachment.eks_cluster]
}

resource "aws_eks_access_entry" "runner" {
  count = var.eks ? 1 : 0

  cluster_name  = aws_eks_cluster.this[0].name
  principal_arn = aws_iam_role.runner.arn
  type          = "STANDARD"
}

resource "aws_eks_access_policy_association" "runner" {
  count = var.eks ? 1 : 0

  cluster_name  = aws_eks_cluster.this[0].name
  principal_arn = aws_iam_role.runner.arn
  # An EKS *access policy*, not an IAM policy, and the two ARN namespaces
  # look alike enough to swap by hand. `arn:aws:iam::aws:policy/...` is
  # rejected as `InvalidParameterException: The policyArn parameter format is
  # not valid` -- a live failure on 2026-09-02. Cluster access policies are
  # only ever `arn:aws:eks::aws:cluster-access-policy/<name>`, and they are not
  # attachable to a role: they exist solely to be associated with an access
  # entry.
  policy_arn = "arn:aws:eks::aws:cluster-access-policy/AmazonEKSClusterAdminPolicy"

  access_scope {
    type = "cluster"
  }

  depends_on = [aws_eks_access_entry.runner]
}

data "aws_iam_policy_document" "eks_node_assume" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "eks_node" {
  count = var.eks ? 1 : 0

  name_prefix        = "yesno-e2e-node-"
  assume_role_policy = data.aws_iam_policy_document.eks_node_assume.json
}

# These are the node's own permissions, and the list no longer includes
# `AmazonEBSCSIDriverPolicy`. That driver's controller has its own web-identity
# role below, and its node plugin makes no AWS API calls -- so granting the
# instance role EBS rights would only hand them to every pod on the node, which
# the driver's documentation calls out as the reason not to use this path.
#
# The original note here claimed the CSI controllers reach the instance
# credentials because managed node groups default the IMDS hop limit to 2. That
# was wrong, and it cost the arm's first provisioning attempt: IMDSv2 defaults
# to a hop limit of **1**, one short of a container.
#
# `AmazonEFSCSIDriverPolicy` stays. It is unused -- this stack binds EFS
# statically, by volume handle, so that controller never calls the API -- but
# it is what makes the arrangement uniform if a later change provisions EFS
# access points dynamically.
#
# An empty `for_each` rather than a `count`, because the two cannot coexist
# on one resource. It carries the same condition and means the same thing.
resource "aws_iam_role_policy_attachment" "eks_node" {
  for_each = var.eks ? toset([
    "arn:aws:iam::aws:policy/AmazonEKSWorkerNodePolicy",
    "arn:aws:iam::aws:policy/AmazonEKS_CNI_Policy",
    "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly",
    "arn:aws:iam::aws:policy/service-role/AmazonEFSCSIDriverPolicy",
  ]) : toset([])

  role       = aws_iam_role.eks_node[0].name
  policy_arn = each.value
}

# One node. The Job is a copy of a small database and runs once; a second node
# would only add an instance hour to every run of the gate.
resource "aws_eks_node_group" "this" {
  count = var.eks ? 1 : 0

  cluster_name    = aws_eks_cluster.this[0].name
  node_group_name = local.eks_name
  node_role_arn   = aws_iam_role.eks_node[0].arn
  subnet_ids      = [aws_subnet.runner.id]
  ami_type        = local.node_ami_type
  instance_types  = [local.instance_type]

  scaling_config {
    desired_size = 1
    max_size     = 1
    min_size     = 1
  }

  depends_on = [aws_iam_role_policy_attachment.eks_node]
}

# The EBS CSI driver restores the snapshot into a volume; the snapshot
# controller is what makes `VolumeSnapshot` and `VolumeSnapshotContent` exist
# at all — EKS does not install its CRDs by default, and the archiver's
# pre-provisioned-snapshot path is written entirely in terms of them. The EFS
# driver backs the one shared RWX claim.
resource "aws_eks_addon" "snapshot_controller" {
  count = var.eks ? 1 : 0

  cluster_name = aws_eks_cluster.this[0].name
  addon_name   = "snapshot-controller"

  depends_on = [aws_eks_node_group.this]
}

# The EBS CSI driver is the one add-on here that needs an AWS identity of
# its own, and it is why this cluster has an OIDC provider at all.
#
# Every other component in this stack takes its permissions from the node
# instance role. That does not work for this driver's *controller*, which runs
# as an ordinary pod: its own documentation states that instance-profile
# credentials require it "to be run in host networking mode, or with a hop
# limit of at least 2", and IMDSv2 defaults to a hop limit of 1, which is
# exactly one short of reaching a container. The add-on then never reports
# ACTIVE.
#
# That is what stalled the arm's first provisioning on 2026-09-02: 20 minutes
# in `CREATING`, then a timeout. The EFS CSI add-on has the same node-role
# arrangement and came up in 36 seconds, which reads as an argument that
# credentials were not the problem and is not one -- this stack binds EFS
# statically, by volume handle, so that controller never calls the API at all.
#
# Web identity removes the dependency on IMDS entirely rather than widening the
# hop limit, which would hand every pod on the node the driver's permissions.
resource "aws_iam_openid_connect_provider" "eks" {
  count = var.eks ? 1 : 0

  url            = aws_eks_cluster.this[0].identity[0].oidc[0].issuer
  client_id_list = ["sts.amazonaws.com"]
}

# Counted, so that the body -- which indexes the cluster -- is not evaluated
# at all when the arm is off.
data "aws_iam_policy_document" "eks_ebs_csi_assume" {
  count = var.eks ? 1 : 0

  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.eks[0].arn]
    }

    # Both conditions are required. `sub` alone would let any service account
    # in any cluster trusting this provider assume the role, and `aud` alone
    # would let any service account in this one.
    condition {
      test     = "StringEquals"
      variable = "${replace(aws_eks_cluster.this[0].identity[0].oidc[0].issuer, "https://", "")}:sub"
      values   = ["system:serviceaccount:kube-system:ebs-csi-controller-sa"]
    }

    condition {
      test     = "StringEquals"
      variable = "${replace(aws_eks_cluster.this[0].identity[0].oidc[0].issuer, "https://", "")}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "eks_ebs_csi" {
  count = var.eks ? 1 : 0

  name_prefix        = "yesno-e2e-ebs-csi-"
  assume_role_policy = data.aws_iam_policy_document.eks_ebs_csi_assume[0].json
}

resource "aws_iam_role_policy_attachment" "eks_ebs_csi" {
  count = var.eks ? 1 : 0

  role       = aws_iam_role.eks_ebs_csi[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonEBSCSIDriverPolicy"
}

resource "aws_eks_addon" "ebs_csi" {
  count = var.eks ? 1 : 0

  cluster_name             = aws_eks_cluster.this[0].name
  addon_name               = "aws-ebs-csi-driver"
  service_account_role_arn = aws_iam_role.eks_ebs_csi[0].arn

  # The other two add-ons reported ACTIVE in 35 and 36 seconds, so ten minutes
  # is a seventeen-fold margin and not a tuning knob. It is here because the
  # default is 20 and this add-on has already spent one of them failing: the
  # whole gate is billable wall-clock, and a stall should cost ten minutes to
  # discover rather than twenty.
  timeouts {
    create = "10m"
  }

  depends_on = [
    aws_eks_addon.snapshot_controller,
    aws_iam_role_policy_attachment.eks_ebs_csi,
  ]
}

resource "aws_eks_addon" "efs_csi" {
  count = var.eks ? 1 : 0

  cluster_name = aws_eks_cluster.this[0].name
  addon_name   = "aws-efs-csi-driver"

  depends_on = [aws_eks_node_group.this]
}

# What lets the runner talk to the Kubernetes API at all.
#
# `endpoint_private_access = true` is why this is needed, and the reason is
# not obvious: with it, EKS associates a Route 53 private hosted zone with the
# VPC, so the cluster's DNS name resolves *inside* the VPC to the control-plane
# ENIs' private addresses rather than to the public endpoint. The runner is in
# the VPC, so it takes that path, and reaching those ENIs needs ingress on the
# **cluster** security group. Node group instances are members of that group and
# so need nothing; the runner carries its own security group and is not.
#
# The symptom is silence, not a rejection. A live run on 2026-09-02 spent 60
# attempts on `curl: (28) Connection timed out` against the API server and
# reported that the snapshot CRDs never appeared -- a message about the wrong
# layer entirely, because dropped packets look like an absent API.
#
# The alternative is turning private access off so the name resolves to the
# public endpoint. That would send control-plane traffic out through the
# internet gateway and back, to reach a cluster three subnets away.
resource "aws_vpc_security_group_ingress_rule" "eks_api_from_runner" {
  count = var.eks ? 1 : 0

  security_group_id            = aws_eks_cluster.this[0].vpc_config[0].cluster_security_group_id
  referenced_security_group_id = aws_security_group.runner.id
  from_port                    = 443
  to_port                      = 443
  ip_protocol                  = "tcp"
}

# The Job mounts the same staging filesystem the ECS task and the runner use,
# so the nodes need the same NFS reachability. Managed node groups place their
# instances in the cluster security group.
resource "aws_vpc_security_group_ingress_rule" "staging_from_nodes" {
  count = var.eks ? 1 : 0

  security_group_id            = aws_security_group.staging.id
  referenced_security_group_id = aws_eks_cluster.this[0].vpc_config[0].cluster_security_group_id
  from_port                    = 2049
  to_port                      = 2049
  ip_protocol                  = "tcp"
}

# What the runner may do to the cluster through the AWS API. Cluster *access*
# is the access entry above; this is only what `aws eks get-token` and a
# diagnosing operator need.
data "aws_iam_policy_document" "runner_eks" {
  count = var.eks ? 1 : 0

  statement {
    sid       = "DescribeCluster"
    actions   = ["eks:DescribeCluster"]
    resources = [aws_eks_cluster.this[0].arn]
  }
}

resource "aws_iam_role_policy" "runner_eks" {
  count = var.eks ? 1 : 0

  name_prefix = "yesno-e2e-eks-"
  role        = aws_iam_role.runner.id
  policy      = data.aws_iam_policy_document.runner_eks[0].json
}


# ---------------------------------------------------------------------------
# The operator arm's identity.
#
# `yesnod` calls EC2 itself under the EBS snapshot backend, so the Pods the
# operator creates need an AWS identity of their own. IRSA rather than the
# node instance profile, for the same reason the EBS CSI driver uses it above:
# instance-role rights are held by *every* pod on the node, and the whole point
# of the split this gate exercises is that the database's authority is its own.
#
# The Pods set `automountServiceAccountToken: false` -- the daemon never
# talks to the Kubernetes API -- and this still works, because the IRSA webhook
# projects its token into a volume of its own that the field does not govern.
# That is precisely the claim this arm exists to test rather than to assume;
# a run where the daemon cannot reach EC2 logs a reconcile failure instead of
# failing to start, which is why `e2e/aws/operator.py` asserts on the log.
# ---------------------------------------------------------------------------
data "aws_iam_policy_document" "eks_yesnod_assume" {
  count = var.eks ? 1 : 0

  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.eks[0].arn]
    }

    condition {
      test     = "StringEquals"
      variable = "${replace(aws_eks_cluster.this[0].identity[0].oidc[0].issuer, "https://", "")}:sub"
      values   = ["system:serviceaccount:${local.eks_operator_namespace}:${local.eks_operator_account}"]
    }

    condition {
      test     = "StringEquals"
      variable = "${replace(aws_eks_cluster.this[0].identity[0].oidc[0].issuer, "https://", "")}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

# Exactly what deferred materialization needs and nothing else.
#
# No `ec2:CreateVolume`, `AttachVolume`, `DetachVolume` or `DeleteVolume`:
# those belong to *local* materialization, which the operator does not generate
# because the Pod it builds drops every capability and so could not mount the
# result. A role that carried them would let this arm pass while the mode it
# claims to test was not the mode in effect.
#
# No `ec2:DescribeVolumes` either. The daemon identifies its own volume from
# the NVMe serial under `/sys/class/block` rather than by asking EC2, and in
# deferred mode it refuses to snapshot when it cannot -- so granting the call
# would hide exactly the in-Pod failure this arm is here to catch.
data "aws_iam_policy_document" "eks_yesnod" {
  count = var.eks ? 1 : 0

  statement {
    actions   = ["ec2:DescribeSnapshots"]
    resources = ["*"]
  }

  statement {
    actions = ["ec2:CreateSnapshot", "ec2:CreateTags"]
    resources = [
      local.ec2_volume_arn,
      local.ec2_snapshot_arn,
    ]
  }

  statement {
    actions   = ["ec2:DeleteSnapshot"]
    resources = [local.ec2_snapshot_arn]
  }
}

resource "aws_iam_role" "eks_yesnod" {
  count = var.eks ? 1 : 0

  name_prefix        = "yesno-e2e-yesnod-"
  assume_role_policy = data.aws_iam_policy_document.eks_yesnod_assume[0].json
}

resource "aws_iam_role_policy" "eks_yesnod" {
  count = var.eks ? 1 : 0

  name_prefix = "yesno-e2e-yesnod-"
  role        = aws_iam_role.eks_yesnod[0].id
  policy      = data.aws_iam_policy_document.eks_yesnod[0].json
}
