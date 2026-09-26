output "ecr_repository_url" {
  value = aws_ecr_repository.aegis.repository_url
}

output "binance_whitelist_ip" {
  description = "Restrict the Binance API key to this address."
  value       = aws_eip.aegis.public_ip
}

output "instance_id" {
  value = aws_instance.aegis.id
}
