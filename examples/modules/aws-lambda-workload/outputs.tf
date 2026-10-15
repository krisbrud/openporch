output "function_name" {
  value = aws_lambda_function.this.function_name
}

output "function_arn" {
  value = aws_lambda_function.this.arn
}

output "role_arn" {
  value = aws_iam_role.this.arn
}

output "region" {
  value = local.region
}

# Only with an `http` trigger.
output "url" {
  value = try(aws_lambda_function_url.this[0].function_url, null)
}
