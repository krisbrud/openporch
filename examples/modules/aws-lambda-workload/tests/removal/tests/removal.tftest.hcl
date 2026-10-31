# Applies against the account or emulator the environment points at.

provider "aws" {
  s3_use_path_style = true
}

# The queue and the artifact first: the workload's trigger needs the queue's
# ARN at plan time, as it has when openporch provisions the queue first.
run "queue" {
  variables {
    workload = false
  }
}

run "deploy" {
  variables {
    workload = true
  }

  assert {
    condition     = output.function_name == "shop-test-worker" && startswith(output.url, "http")
    error_message = "The workload is a function with a URL."
  }
}

run "remove" {
  variables {
    workload = false
  }

  assert {
    condition     = output.function_name == "" && output.url == ""
    error_message = "The function and its URL are gone from the state."
  }
  assert {
    condition     = endswith(output.queue_arn, ":openporch-test-worker-inbox")
    error_message = "The queue stays in the state."
  }
}

run "check" {
  module {
    source = "./check"
  }

  variables {
    queue     = "openporch-test-worker-inbox"
    queue_arn = run.remove.queue_arn
  }

  assert {
    condition     = !contains(coalesce(data.aws_lambda_functions.all.function_names, []), "shop-test-worker")
    error_message = "The function was deleted."
  }
  assert {
    condition     = data.aws_sqs_queue.queue.arn == var.queue_arn
    error_message = "The queue still exists, the one deployed beside the workload."
  }
}

# Deploying the workload again creates its schedule and URL under the same
# names, which an emulator or AWS refuses while the old ones exist.
run "redeploy" {
  variables {
    workload = true
  }

  assert {
    condition     = output.function_name == "shop-test-worker" && endswith(output.url, "/")
    error_message = "The workload comes back with its URL."
  }
}
