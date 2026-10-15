output "role_arn" {
  value       = aws_iam_role.openporch.arn
  description = "The role openporch assumes."
}

output "artifact_bucket" {
  value       = aws_s3_bucket.artifacts.bucket
  description = "Where CI uploads function zips."
}

output "cloud_account" {
  value       = openporch_cloud_account.aws.id
  description = "The cloud account the definitions use."
}
