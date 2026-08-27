variable "region" {
  description = "AWS region in which the disposable EBS fixture runs."
  type        = string

  validation {
    condition     = can(regex("^[a-z]{2}(-gov)?-[a-z]+-[0-9]+$", var.region))
    error_message = "region must be an AWS region name."
  }
}

variable "run_id" {
  description = "Unique lowercase identifier used for names, tags, and exact cleanup."
  type        = string

  validation {
    condition     = length(var.run_id) <= 32 && can(regex("^[a-z0-9][a-z0-9-]+$", var.run_id))
    error_message = "run_id must be 2-32 lowercase letters, digits, or hyphens."
  }
}

variable "architecture" {
  description = "Runner architecture selected by scripts/gate-aws.sh from the Docker host."
  type        = string
  default     = "x86_64"

  validation {
    condition     = contains(["x86_64", "arm64"], var.architecture)
    error_message = "architecture must be x86_64 or arm64."
  }
}

variable "instance_type" {
  description = "Optional override for the EC2 runner and the EKS node group; defaults to a burstable t4g/t3 by architecture."
  type        = string
  default     = ""
}

variable "eks" {
  description = "Provision the EKS deferred-materialization arm; scripts/gate-aws.sh passes YESNO_AWS_EKS through."
  type        = bool
  default     = true
}

variable "source_volume_gib" {
  description = "Size of the whole-volume ext4 EBS source fixture."
  type        = number
  default     = 8

  validation {
    condition     = var.source_volume_gib >= 4
    error_message = "source_volume_gib must be at least 4 GiB."
  }
}
