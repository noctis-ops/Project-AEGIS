data "aws_ssm_parameter" "al2023_ami" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
}

data "aws_caller_identity" "current" {}

resource "aws_ecr_repository" "aegis" {
  name                 = "project-aegis"
  image_tag_mutability = "IMMUTABLE"

  image_scanning_configuration {
    scan_on_push = true
  }
}

resource "aws_ecr_lifecycle_policy" "aegis" {
  repository = aws_ecr_repository.aegis.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Keep the latest 20 immutable releases"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 20
      }
      action = { type = "expire" }
    }]
  })
}

resource "aws_security_group" "aegis" {
  name_prefix = "aegis-egress-only-"
  description = "AEGIS has no public inbound service; administration uses SSM"
  vpc_id      = var.vpc_id

  egress {
    description = "TLS APIs and ECR"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = { Name = "aegis-egress-only" }
}

resource "aws_iam_role" "aegis" {
  name_prefix = "aegis-instance-"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.aegis.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_role_policy" "runtime" {
  name = "aegis-runtime-minimum"
  role = aws_iam_role.aegis.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["secretsmanager:GetSecretValue"]
        Resource = var.secrets_arn
      },
      {
        Effect = "Allow"
        Action = [
          "ecr:GetAuthorizationToken"
        ]
        Resource = "*"
      },
      {
        Effect = "Allow"
        Action = [
          "ecr:BatchCheckLayerAvailability",
          "ecr:GetDownloadUrlForLayer",
          "ecr:BatchGetImage"
        ]
        Resource = aws_ecr_repository.aegis.arn
      }
    ]
  })
}

resource "aws_iam_instance_profile" "aegis" {
  name_prefix = "aegis-"
  role        = aws_iam_role.aegis.name
}

resource "aws_instance" "aegis" {
  ami                         = data.aws_ssm_parameter.al2023_ami.value
  instance_type               = var.instance_type
  subnet_id                   = var.subnet_id
  associate_public_ip_address = true
  vpc_security_group_ids      = [aws_security_group.aegis.id]
  iam_instance_profile        = aws_iam_instance_profile.aegis.name
  monitoring                  = true
  ebs_optimized               = true

  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }

  root_block_device {
    encrypted   = true
    volume_type = "gp3"
    volume_size = 30
  }

  user_data = templatefile("${path.module}/user-data.sh.tftpl", {
    region      = var.aws_region
    account_id  = data.aws_caller_identity.current.account_id
    repository  = aws_ecr_repository.aegis.name
    image_tag   = var.image_tag
    secrets_arn = var.secrets_arn
  })

  lifecycle {
    create_before_destroy = true
  }

  tags = { Name = "project-aegis" }
}

resource "aws_eip" "aegis" {
  domain   = "vpc"
  instance = aws_instance.aegis.id
  tags     = { Name = "aegis-binance-whitelist" }
}
