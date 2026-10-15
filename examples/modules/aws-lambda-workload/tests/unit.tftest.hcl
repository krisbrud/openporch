# Plans the module with a mocked AWS provider: how it reads the workload
# document, splits secrets out of the environment, translates triggers and
# refuses what Lambda cannot run.

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
  resource = {
    id    = "order-worker"
    class = "aws-lambda"
    spec = {
      function = {
        package = {
          type     = "zip"
          artifact = "s3://acme-artifacts/order-worker/sha256-abc.zip?versionId=v1"
          runtime  = "python3.12"
          handler  = "app.handler"
        }
        variables = { TABLE = "shop-dev-orders" }
        memory    = "256Mi"
        timeout   = "30s"
      }
      deploy = { status = "available", timeout = "5m" }
    }
    dependencies = {
      orders                                                     = { name = "shop-dev-orders", arn = "arn:aws:dynamodb:eu-north-1:123456789012:table/shop-dev-orders" }
      "aws-policy.default#modules.order-worker.externals.orders" = { arn = "arn:aws:iam::123456789012:policy/orders" }
    }
  }
  resource_secrets = {
    spec = { function = { variables = { DB_PASSWORD = "hunter2" } } }
  }
}

run "zip_function" {
  command = plan

  assert {
    condition = (
      aws_lambda_function.this.function_name == "shop-dev-order-worker" &&
      aws_lambda_function.this.package_type == "Zip" &&
      aws_lambda_function.this.runtime == "python3.12" &&
      aws_lambda_function.this.handler == "app.handler"
    )
    error_message = "The function is named after the application, environment and workload, with the package's runtime and handler."
  }
  assert {
    condition = (
      aws_lambda_function.this.s3_bucket == "acme-artifacts" &&
      aws_lambda_function.this.s3_key == "order-worker/sha256-abc.zip" &&
      aws_lambda_function.this.s3_object_version == "v1"
    )
    error_message = "The code is the pinned artifact version."
  }
  assert {
    condition     = aws_lambda_function.this.memory_size == 256 && aws_lambda_function.this.timeout == 30
    error_message = "256Mi and 30s become 256 MB and 30 seconds."
  }
  assert {
    condition     = aws_iam_role_policy_attachment.dependency["aws-policy.default#modules.order-worker.externals.orders"].policy_arn == "arn:aws:iam::123456789012:policy/orders"
    error_message = "The role attaches the table's co-provisioned policy."
  }
  assert {
    condition     = length(aws_iam_role_policy_attachment.dependency) == 1
    error_message = "Only `aws-policy` dependencies are attached."
  }
}

run "secret_variables_are_references" {
  command = plan

  assert {
    condition = (
      aws_lambda_function.this.environment[0].variables["TABLE"] == "shop-dev-orders" &&
      !contains(keys(aws_lambda_function.this.environment[0].variables), "DB_PASSWORD") &&
      aws_lambda_function.this.environment[0].variables["DB_PASSWORD_SECRET_REF"] == "/openporch/shop-dev-order-worker/DB_PASSWORD"
    )
    error_message = "A secret variable reaches the function only as a reference to its parameter."
  }
  assert {
    condition     = aws_ssm_parameter.variable["DB_PASSWORD"].type == "SecureString" && nonsensitive(aws_ssm_parameter.variable["DB_PASSWORD"].value) == "hunter2"
    error_message = "The secret is a SecureString parameter."
  }
  assert {
    condition     = length(aws_secretsmanager_secret.variable) == 0
    error_message = "Parameters are the default."
  }
  assert {
    condition = anytrue([
      for st in data.aws_iam_policy_document.this.statement :
      st.sid == "SecretParameters" && contains(st.actions, "ssm:GetParameter") && contains(st.resources, aws_ssm_parameter.variable["DB_PASSWORD"].arn)
    ])
    error_message = "The role may read the secret's parameter."
  }
  assert {
    condition     = length(data.aws_iam_policy_document.this.statement) == 2
    error_message = "The role may write its logs and read its secrets, nothing else."
  }
}

run "secrets_manager" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        target   = { lambda = { secrets = "secretsmanager", architecture = "arm64" } }
      }
    }
  }

  assert {
    condition     = length(aws_ssm_parameter.variable) == 0 && aws_secretsmanager_secret.variable["DB_PASSWORD"].name == "openporch/shop-dev-order-worker/DB_PASSWORD"
    error_message = "With `secrets: secretsmanager` the secret is a Secrets Manager secret."
  }
  assert {
    condition     = aws_lambda_function.this.environment[0].variables["DB_PASSWORD_SECRET_REF"] == "openporch/shop-dev-order-worker/DB_PASSWORD"
    error_message = "The function gets the secret's name."
  }
  assert {
    condition = anytrue([
      for st in data.aws_iam_policy_document.this.statement :
      st.sid == "Secrets" && st.actions == toset(["secretsmanager:GetSecretValue"]) && contains(st.resources, aws_secretsmanager_secret.variable["DB_PASSWORD"].arn)
    ])
    error_message = "The role may read the secret."
  }
  assert {
    condition     = aws_lambda_function.this.architectures == tolist(["arm64"])
    error_message = "The architecture comes from the target."
  }
}

run "image_from_ecr" {
  command = plan

  variables {
    resource = {
      id   = "api"
      spec = { function = { package = { type = "image", image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/api@sha256:abc" }, memory = "1Gi", timeout = "1m30s" } }
    }
    resource_secrets = {}
  }

  assert {
    condition     = aws_lambda_function.this.package_type == "Image" && aws_lambda_function.this.image_uri == "123456789012.dkr.ecr.eu-north-1.amazonaws.com/api@sha256:abc"
    error_message = "An image function runs the image."
  }
  assert {
    condition     = aws_lambda_function.this.memory_size == 1024 && aws_lambda_function.this.timeout == 90
    error_message = "1Gi and 1m30s become 1024 MB and 90 seconds."
  }
  assert {
    condition     = length(aws_lambda_function.this.environment) == 0
    error_message = "Without variables there is no environment."
  }
}

run "image_outside_ecr" {
  command = plan

  variables {
    resource = {
      id   = "api"
      spec = { function = { package = { type = "image", image = "ghcr.io/acme/api:1" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
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

  expect_failures = [aws_lambda_function.this]
}

run "container_with_web_adapter_port" {
  command = plan

  variables {
    resource = {
      id = "web"
      spec = {
        containers = {
          main = {
            image     = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/web:1"
            command   = ["/app/server"]
            args      = ["--port", "8080"]
            variables = { LOG_LEVEL = "info" }
          }
        }
        service  = { ports = { http = { port = 80, targetPort = 8080 } } }
        triggers = { web = { http = { port = "http", auth = "none" } } }
      }
    }
    resource_secrets = { spec = { containers = { main = { variables = { API_KEY = "k-1" } } } } }
  }

  assert {
    condition = (
      aws_lambda_function.this.package_type == "Image" &&
      aws_lambda_function.this.image_config[0].entry_point == tolist(["/app/server"]) &&
      aws_lambda_function.this.image_config[0].command == tolist(["--port", "8080"])
    )
    error_message = "The container's command and args override the image's entry point and command."
  }
  assert {
    condition     = aws_lambda_function.this.environment[0].variables["AWS_LWA_PORT"] == "8080" && aws_lambda_function.this.environment[0].variables["LOG_LEVEL"] == "info"
    error_message = "The service's target port is exported as AWS_LWA_PORT."
  }
  assert {
    condition     = aws_lambda_function.this.environment[0].variables["API_KEY_SECRET_REF"] == "/openporch/shop-dev-web/API_KEY"
    error_message = "A container's secret variable is a reference too."
  }
  assert {
    condition     = aws_lambda_function_url.this[0].authorization_type == "NONE" && length(aws_lambda_permission.url) == 1 && length(aws_lambda_permission.url_invoke) == 1
    error_message = "`auth: none` makes a public function URL that anyone may invoke."
  }
}

run "two_containers" {
  command = plan

  variables {
    resource = {
      id = "web"
      spec = { containers = {
        a = { image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/a:1" }
        b = { image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/b:1" }
      } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "container_files" {
  command = plan

  variables {
    resource = {
      id = "web"
      spec = { containers = { main = {
        image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/web:1"
        files = { "/etc/app.conf" = { content = "x" } }
      } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "inferred_url_is_not_public" {
  command = plan

  variables {
    resource = {
      id = "web"
      spec = {
        containers = { main = { image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/web:1" } }
        service    = { ports = { http = { port = 80, targetPort = 8080 } } }
        triggers   = { http = { http = { port = "http" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "iam_url" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { api = { http = { auth = "iam" } } }
      }
    }
    resource_secrets = {}
  }

  assert {
    condition     = aws_lambda_function_url.this[0].authorization_type == "AWS_IAM" && length(aws_lambda_permission.url) == 0
    error_message = "`auth: iam` makes a URL that needs signed requests, with no public permission."
  }
  assert {
    condition     = output.url == "https://mock.lambda-url.eu-north-1.on.aws/"
    error_message = "The module's `url` output is the function URL."
  }
}

run "schedules" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = {
          nightly  = { schedule = { cron = "0 3 * * *", timezone = "Europe/Oslo" } }
          weekdays = { schedule = { cron = "30 7 * * 1-5", input = { report = "daily" } } }
          sundays  = { schedule = { cron = "*/15 * * * 0,6" } }
          monthly  = { schedule = { cron = "0 0 1 */3 *" } }
          names    = { schedule = { cron = "0 9 * jan mon-fri" } }
          steps    = { schedule = { cron = "5-55/10 8-18 1-15 * *" } }
        }
      }
    }
    resource_secrets = {}
  }

  assert {
    condition = (
      aws_scheduler_schedule.this["nightly"].schedule_expression == "cron(0 3 * * ? *)" &&
      aws_scheduler_schedule.this["nightly"].schedule_expression_timezone == "Europe/Oslo"
    )
    error_message = "A nightly cron runs at 03:00 in its time zone."
  }
  assert {
    condition     = aws_scheduler_schedule.this["weekdays"].schedule_expression == "cron(30 7 ? * 2-6 *)" && aws_scheduler_schedule.this["weekdays"].schedule_expression_timezone == "UTC"
    error_message = "Days of the week are numbered from Sunday = 1, and UTC is the default time zone."
  }
  assert {
    condition     = aws_scheduler_schedule.this["weekdays"].target[0].input == jsonencode({ report = "daily" })
    error_message = "The schedule's input is the event."
  }
  assert {
    condition     = aws_scheduler_schedule.this["sundays"].schedule_expression == "cron(0/15 * ? * 1,7 *)"
    error_message = "Steps and lists translate."
  }
  assert {
    condition     = aws_scheduler_schedule.this["monthly"].schedule_expression == "cron(0 0 1 1/3 ? *)"
    error_message = "Day-of-month schedules leave day-of-week open."
  }
  assert {
    condition     = aws_scheduler_schedule.this["names"].schedule_expression == "cron(0 9 ? JAN MON-FRI *)"
    error_message = "Month and day names stay names."
  }
  assert {
    condition     = aws_scheduler_schedule.this["steps"].schedule_expression == "cron(5-55/10 8-18 1-15 * ? *)"
    error_message = "Ranges with steps translate."
  }
  assert {
    condition = (
      jsondecode(aws_iam_role_policy.scheduler[0].policy).Statement[0].Action == "lambda:InvokeFunction" &&
      jsondecode(aws_iam_role_policy.scheduler[0].policy).Statement[0].Resource[0] == aws_lambda_function.this.arn &&
      length(jsondecode(aws_iam_role_policy.scheduler[0].policy).Statement) == 1
    )
    error_message = "The scheduler's role may only invoke the function."
  }
}

run "cron_with_both_days" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { odd = { schedule = { cron = "0 3 1 * 1" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "cron_macro" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { daily = { schedule = { cron = "@daily" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "queue_trigger" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = {
          inbox = { queue = { resource = "inbox", batchSize = 5, outputs = { arn = "arn:aws:sqs:eu-north-1:123456789012:inbox" } } }
        }
      }
    }
    resource_secrets = {}
  }

  assert {
    condition = (
      aws_lambda_event_source_mapping.queue["inbox"].event_source_arn == "arn:aws:sqs:eu-north-1:123456789012:inbox" &&
      aws_lambda_event_source_mapping.queue["inbox"].batch_size == 5
    )
    error_message = "The queue invokes the function in batches of 5."
  }
  assert {
    condition = anytrue([
      for st in data.aws_iam_policy_document.this.statement :
      st.sid == "TriggerQueues" && st.resources == toset(["arn:aws:sqs:eu-north-1:123456789012:inbox"]) &&
      contains(st.actions, "sqs:ReceiveMessage") && contains(st.actions, "sqs:DeleteMessage")
    ])
    error_message = "The role may receive and delete the queue's messages."
  }
}

run "topic_trigger" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { news = { topic = { resource = "news" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "two_urls" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { a = { http = { auth = "iam" } }, b = { http = { auth = "iam" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "limits" {
  command = plan

  variables {
    resource = {
      id   = "big"
      spec = { function = { package = { type = "zip", artifact = "s3://b/k.zip", runtime = "python3.12", handler = "a.h" }, memory = "20Gi", timeout = "20m" } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "artifact_not_in_s3" {
  command = plan

  variables {
    resource = {
      id   = "worker"
      spec = { function = { package = { type = "zip", artifact = "https://example.com/k.zip", runtime = "python3.12", handler = "a.h" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "provisioned_concurrency" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = {
          package     = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" }
          concurrency = { reserved = 10, provisioned = 2 }
        }
        triggers = { nightly = { schedule = { cron = "0 3 * * *" } } }
      }
    }
    resource_secrets = {}
  }

  assert {
    condition = (
      aws_lambda_function.this.publish &&
      aws_lambda_function.this.reserved_concurrent_executions == 10 &&
      aws_lambda_provisioned_concurrency_config.live[0].provisioned_concurrent_executions == 2
    )
    error_message = "Provisioned concurrency publishes versions behind the `live` alias."
  }
  assert {
    condition     = aws_scheduler_schedule.this["nightly"].target[0].arn == "arn:aws:lambda:eu-north-1:123456789012:function:mock:live"
    error_message = "Triggers invoke the alias."
  }
}

run "cron_out_of_range" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = {
          a = { schedule = { cron = "99 3 * * *" } }
          b = { schedule = { cron = "0 3 * * xyz" } }
          c = { schedule = { cron = "0 3 * * 5-1" } }
          d = { schedule = { cron = "0 3 * 13 *" } }
          e = { schedule = { cron = "*/0 3 0 * *" } }
        }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "unknown_timezone" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { a = { schedule = { cron = "0 3 * * *", timezone = "Mars/Base" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "url_with_a_path" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { api = { http = { auth = "iam", path = "/api" } } }
      }
    }
    resource_secrets = {}
  }

  expect_failures = [terraform_data.triggers]
}

run "provisioned_above_reserved" {
  command = plan

  variables {
    resource = {
      id   = "order-worker"
      spec = { function = { package = { type = "zip", artifact = "s3://b/k.zip", runtime = "python3.12", handler = "a.h" }, concurrency = { reserved = 1, provisioned = 5 } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "reserved_not_a_number" {
  command = plan

  variables {
    resource = {
      id   = "order-worker"
      spec = { function = { package = { type = "zip", artifact = "s3://b/k.zip", runtime = "python3.12", handler = "a.h" }, concurrency = { reserved = "lots" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "unknown_package_type" {
  command = plan

  variables {
    resource = {
      id   = "order-worker"
      spec = { function = { package = { type = "jar", image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/api:1" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "image_function_without_image" {
  command = plan

  variables {
    resource = {
      id   = "order-worker"
      spec = { function = { package = { type = "image" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "zip_without_handler" {
  command = plan

  variables {
    resource = {
      id   = "order-worker"
      spec = { function = { package = { type = "zip", artifact = "s3://b/k.zip", runtime = "python3.12" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "reserved_variable_name" {
  command = plan

  variables {
    resource = {
      id   = "order-worker"
      spec = { function = { package = { type = "zip", artifact = "s3://b/k.zip", runtime = "python3.12", handler = "a.h" }, variables = { AWS_REGION = "us-east-1" } } }
    }
    resource_secrets = {}
  }

  expect_failures = [aws_lambda_function.this]
}

run "secret_outside_variables" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { nightly = { schedule = { cron = "0 3 * * *", input = { token = "" } } } }
      }
    }
    resource_secrets = { spec = { triggers = { nightly = { schedule = { input = { token = "t-1" } } } } } }
  }

  expect_failures = [aws_lambda_function.this]
}

run "secret_in_container_args" {
  command = plan

  variables {
    resource = {
      id = "order-worker"
      spec = { containers = { main = {
        image = "123456789012.dkr.ecr.eu-north-1.amazonaws.com/web:1"
        args  = ["--token", null]
      } } }
    }
    resource_secrets = { spec = { containers = { main = { args = { "1" = "t-1" } } } } }
  }

  expect_failures = [aws_lambda_function.this]
}

run "long_names_stay_unique" {
  command = plan

  variables {
    app = "a-long-application-name"
    env = "development"
    resource = {
      id = "a-workload-with-long-id"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { nightly = { schedule = { cron = "0 3 * * *" } }, nightly-reports = { schedule = { cron = "0 4 * * *" } } }
      }
    }
    resource_secrets = {}
  }

  assert {
    condition = (
      length(aws_iam_role.this.name) <= 64 && length(aws_iam_role.scheduler[0].name) <= 64 &&
      aws_iam_role.this.name != aws_iam_role.scheduler[0].name
    )
    error_message = "Long role names are shortened and stay distinct."
  }
  assert {
    condition = (
      length(aws_scheduler_schedule.this["nightly"].name) <= 64 && length(aws_scheduler_schedule.this["nightly-reports"].name) <= 64 &&
      aws_scheduler_schedule.this["nightly"].name != aws_scheduler_schedule.this["nightly-reports"].name
    )
    error_message = "Long schedule names are shortened and stay distinct."
  }
}

run "bounded_roles" {
  command = plan

  variables {
    iam_path             = "/openporch/"
    permissions_boundary = "arn:aws:iam::123456789012:policy/openporch-boundary"
    resource = {
      id = "order-worker"
      spec = {
        function = { package = { type = "zip", artifact = "s3://acme-artifacts/w.zip", runtime = "python3.12", handler = "app.handler" } }
        triggers = { nightly = { schedule = { cron = "0 3 * * *" } } }
      }
    }
    resource_secrets = {}
  }

  assert {
    condition = (
      aws_iam_role.this.path == "/openporch/" && aws_iam_role.scheduler[0].path == "/openporch/" &&
      aws_iam_role.this.permissions_boundary == "arn:aws:iam::123456789012:policy/openporch-boundary" &&
      aws_iam_role.scheduler[0].permissions_boundary == "arn:aws:iam::123456789012:policy/openporch-boundary"
    )
    error_message = "Both roles are created under the path, with the boundary."
  }
}
