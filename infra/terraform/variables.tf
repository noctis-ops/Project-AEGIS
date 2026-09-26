variable "aws_region" {
  description = "Tokyo keeps the host near Binance infrastructure. Measure before production."
  type        = string
  default     = "ap-northeast-1"
}

variable "instance_type" {
  type    = string
  default = "c6i.xlarge"
}

variable "subnet_id" {
  description = "Existing public subnet. No inbound port is opened; use SSM."
  type        = string
}

variable "vpc_id" {
  type = string
}

variable "secrets_arn" {
  description = "Secrets Manager ARN containing runtime keys; never pass the secret value."
  type        = string
}

variable "image_tag" {
  type    = string
  default = "approved"
}
