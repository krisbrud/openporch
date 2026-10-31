# expect-error: Lambda container images must come from ECR in the function's region (eu-north-1); `123456789012.dkr.ecr.us-east-1.amazonaws.com/api:1` does not.
#
# Run by ../../../test.sh, which expects this plan to fail with the message
# above: `expect_failures` in unit.tftest.hcl cannot check what it says.

mock_provider "aws" {
  mock_data "aws_region" {
    defaults = { region = "eu-north-1" }
  }
  mock_data "aws_caller_identity" {
    defaults = { account_id = "123456789012" }
  }
  mock_data "aws_iam_policy_document" {
    defaults = { json = "{}" }
  }
  mock_resource "aws_ssm_parameter" {
    defaults = { arn = "arn:aws:ssm:eu-north-1:123456789012:parameter/mock" }
  }
  mock_resource "aws_lambda_function" {
    defaults = { arn = "arn:aws:lambda:eu-north-1:123456789012:function:mock" }
  }
  mock_resource "aws_lambda_alias" {
    defaults = { arn = "arn:aws:lambda:eu-north-1:123456789012:function:mock:live" }
  }
  mock_resource "aws_iam_role" {
    defaults = { arn = "arn:aws:iam::123456789012:role/mock" }
  }
  mock_resource "aws_lambda_function_url" {
    defaults = { function_url = "https://mock.lambda-url.eu-north-1.on.aws/" }
  }
  mock_resource "aws_sqs_queue" {
    defaults = {}
  }
  mock_resource "aws_cloudwatch_log_group" {
    defaults = { arn = "arn:aws:logs:eu-north-1:123456789012:log-group:mock" }
  }
}

variables {
  app = "shop"
  env = "dev"
}

run "image_in_another_region" {
  command = plan

  variables {
    resource = {
      id   = "api"
      spec = { function = { package = { type = "image", image = "123456789012.dkr.ecr.us-east-1.amazonaws.com/api:1" } } }
    }
    resource_secrets = {}
  }
}
