# AEGIS AWS infrastructure

This module creates an immutable ECR repository, an egress-only security group, least-privilege EC2/SSM role, encrypted c6i.xlarge host in Tokyo, and a stable Elastic IP for the Binance key whitelist.

The secret **value** must already exist in Secrets Manager. Pass only its ARN. Terraform state must use an encrypted remote backend with locking; backend configuration is intentionally organization-specific.

```bash
terraform init
terraform plan -var subnet_id=subnet-... -var vpc_id=vpc-... -var secrets_arn=arn:aws:secretsmanager:...
terraform apply <reviewed-plan>
```

The boot template starts **shadow mode**. Changing to live must be a reviewed IaC release after all gates in `docs/OPERATIONS.ar.md`; do not edit the instance manually.
