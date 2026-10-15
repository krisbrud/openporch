# Triggers: a function URL, schedules and queue event source mappings.

locals {
  triggers = try(local.spec.triggers, {})
  # Each trigger is `{<kind>: {...}}`.
  trigger_kinds = { for name, t in local.triggers : name => try(keys(t)[0], "") }
  unsupported   = [for name, kind in local.trigger_kinds : "`${name}` (${kind})" if !contains(["http", "schedule", "queue"], kind)]

  http_triggers = { for name, t in local.triggers : name => t.http if local.trigger_kinds[name] == "http" }
  http          = length(local.http_triggers) == 1 ? values(local.http_triggers)[0] : null
  http_auth     = local.http == null ? null : try(local.http.auth, null)

  queue_triggers = {
    for name, t in local.triggers : name => {
      arn        = try(t.queue.outputs.arn, null)
      batch_size = try(tonumber(t.queue.batchSize), 10)
    } if local.trigger_kinds[name] == "queue"
  }

  # A 5-field cron expression as an EventBridge Scheduler `cron(...)`, which
  # has a year field and needs `?` in one of day-of-month and day-of-week, and
  # numbers days of the week from 1 (Sunday). Anything that would have to be
  # approximated is refused instead: `null`.
  schedules = {
    for name, t in local.triggers : name => {
      cron     = t.schedule.cron
      timezone = try(t.schedule.timezone, "UTC")
      input    = try(t.schedule.input, null)
      fields   = regexall("\\S+", t.schedule.cron)
    } if local.trigger_kinds[name] == "schedule"
  }
  # Each field's range and names: minute, hour, day of month, month, day of
  # week.
  cron_fields = [
    { min = 0, max = 59, names = {} },
    { min = 0, max = 23, names = {} },
    { min = 1, max = 31, names = {} },
    { min = 1, max = 12, names = { JAN = 1, FEB = 2, MAR = 3, APR = 4, MAY = 5, JUN = 6, JUL = 7, AUG = 8, SEP = 9, OCT = 10, NOV = 11, DEC = 12 } },
    { min = 0, max = 7, names = { SUN = 0, MON = 1, TUE = 2, WED = 3, THU = 4, FRI = 5, SAT = 6 } },
  ]
  # A list item: `*`, a value or a range, with an optional step.
  cron_item = "^(\\*|([0-9]+|[A-Za-z]{3})(-([0-9]+|[A-Za-z]{3}))?)(/([0-9]+))?$"
  cron_valid = {
    for name, s in local.schedules : name => length(s.fields) == 5 && alltrue([
      for i, f in s.fields : alltrue([
        for item in split(",", f) : try(
          regex(local.cron_item, item)[0] == "*" ? tonumber(coalesce(regex(local.cron_item, item)[5], "1")) >= 1 : (
            alltrue([
              for v in compact([regex(local.cron_item, item)[1], regex(local.cron_item, item)[3]]) :
              try(tonumber(v), lookup(local.cron_fields[i].names, upper(v), -1)) >= local.cron_fields[i].min &&
              try(tonumber(v), lookup(local.cron_fields[i].names, upper(v), -1)) <= local.cron_fields[i].max
            ]) &&
            try(tonumber(regex(local.cron_item, item)[1]), lookup(local.cron_fields[i].names, upper(regex(local.cron_item, item)[1]), -1)) <=
            try(tonumber(coalesce(regex(local.cron_item, item)[3], regex(local.cron_item, item)[1])), lookup(local.cron_fields[i].names, upper(coalesce(regex(local.cron_item, item)[3], regex(local.cron_item, item)[1])), -1)) &&
            tonumber(coalesce(regex(local.cron_item, item)[5], "1")) >= 1
          ),
          false
        )
      ])
    ])
  }
  schedule_expressions = {
    for name, s in local.schedules : name => (
      local.cron_valid[name]
      && (s.fields[2] == "*" || s.fields[4] == "*")
      && !strcontains(s.fields[4], "/")
      && alltrue([
        for t in split(",", s.fields[4]) :
        can(regex("^\\*$|^[0-7]$|^[0-6]-[0-6]$|^[A-Za-z]{3}(-[A-Za-z]{3})?$", t))
      ])
      ? format(
        "cron(%s %s %s %s %s *)",
        replace(s.fields[0], "/^\\*\\//", "0/"),
        replace(s.fields[1], "/^\\*\\//", "0/"),
        s.fields[4] == "*" ? replace(s.fields[2], "/^\\*\\//", "1/") : "?",
        upper(replace(s.fields[3], "/^\\*\\//", "1/")),
        s.fields[4] == "*" ? "?" : join(",", [
          for t in split(",", s.fields[4]) : (
            can(regex("^[0-7]$", t)) ? tostring(tonumber(t) % 7 + 1) :
            can(regex("^[0-6]-[0-6]$", t)) ? format("%d-%d", tonumber(split("-", t)[0]) + 1, tonumber(split("-", t)[1]) + 1) :
            upper(t)
          )
        ])
      )
      : null
    )
  }
  # IANA time zone names, by their areas.
  bad_timezones = [
    for name, s in local.schedules : "`${name}` (${s.timezone})"
    if !can(regex("^(UTC|(Africa|America|Antarctica|Arctic|Asia|Atlantic|Australia|Europe|Indian|Pacific|Etc)/[A-Za-z0-9_+-]+(/[A-Za-z0-9_+-]+)?)$", s.timezone))
  ]
  # Names too long for Scheduler are shortened with a hash, so they stay
  # unique.
  schedule_names = {
    for name, s in local.schedules : name => (
      length("${local.name}-${name}") <= 64 ? "${local.name}-${name}" : "${substr("${local.name}-${name}", 0, 55)}-${substr(sha1("${local.name}-${name}"), 0, 8)}"
    )
  }
}

resource "terraform_data" "triggers" {
  input = local.trigger_kinds

  lifecycle {
    precondition {
      condition     = length(local.unsupported) == 0
      error_message = "Triggers not supported on Lambda in this milestone: ${join(", ", local.unsupported)}."
    }
    precondition {
      condition     = length(local.http_triggers) <= 1
      error_message = "A Lambda function has one URL, so a workload has at most one `http` trigger."
    }
    precondition {
      condition     = local.http == null || contains(["none", "iam"], coalesce(local.http_auth, "-"))
      error_message = "The `http` trigger `${try(keys(local.http_triggers)[0], "-")}` needs an explicit `auth: none` (a public URL) or `auth: iam`; a URL is never made public by default."
    }
    precondition {
      condition     = alltrue([for name, e in local.schedule_expressions : e != null])
      error_message = "Schedules need a 5-field cron expression with values in range that restricts at most one of day-of-month and day-of-week, without steps in day-of-week: ${join(", ", [for name, e in local.schedule_expressions : "`${name}` (${local.schedules[name].cron})" if e == null])}."
    }
    precondition {
      condition     = length(local.bad_timezones) == 0
      error_message = "Schedules need an IANA time zone, such as `Europe/Oslo`, or `UTC`: ${join(", ", local.bad_timezones)}."
    }
    precondition {
      condition     = local.http == null || contains(["/", ""], coalesce(try(local.http.path, null), "/"))
      error_message = "A function URL has no path; the function sees every path, so the `http` trigger takes none."
    }
    precondition {
      condition     = alltrue([for t in values(local.queue_triggers) : t.arn != null && t.batch_size >= 1 && t.batch_size <= 10000])
      error_message = "A `queue` trigger needs an `sqs` resource with an `arn` output, and a `batchSize` of 1 to 10000."
    }
  }
}

resource "aws_lambda_function_url" "this" {
  count              = local.http == null ? 0 : 1
  function_name      = aws_lambda_function.this.function_name
  qualifier          = local.qualifier
  authorization_type = local.http_auth == "none" ? "NONE" : "AWS_IAM"
  depends_on         = [terraform_data.triggers]
}

# A public URL needs permission for anyone to call it.
resource "aws_lambda_permission" "url" {
  count                  = local.http_auth == "none" ? 1 : 0
  statement_id           = "PublicFunctionUrl"
  action                 = "lambda:InvokeFunctionUrl"
  function_name          = aws_lambda_function.this.function_name
  qualifier              = local.qualifier
  principal              = "*"
  function_url_auth_type = "NONE"
}

resource "aws_lambda_permission" "url_invoke" {
  count                    = local.http_auth == "none" ? 1 : 0
  statement_id             = "PublicFunctionUrlInvoke"
  action                   = "lambda:InvokeFunction"
  function_name            = aws_lambda_function.this.function_name
  qualifier                = local.qualifier
  principal                = "*"
  invoked_via_function_url = true
}

resource "aws_lambda_event_source_mapping" "queue" {
  for_each         = local.queue_triggers
  event_source_arn = each.value.arn
  function_name    = local.invoke_arn
  batch_size       = each.value.batch_size
  depends_on       = [terraform_data.triggers, aws_iam_role_policy.this]
}

data "aws_iam_policy_document" "scheduler_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["scheduler.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [data.aws_caller_identity.current.account_id]
    }
  }
}

resource "aws_iam_role" "scheduler" {
  count                = length(local.schedules) > 0 ? 1 : 0
  name                 = length("${local.name}-scheduler") <= 64 ? "${local.name}-scheduler" : "${substr(local.name, 0, 44)}-${substr(sha1(local.name), 0, 8)}-scheduler"
  assume_role_policy   = data.aws_iam_policy_document.scheduler_assume.json
  path                 = var.iam_path
  permissions_boundary = var.permissions_boundary
  tags                 = var.tags
}

resource "aws_iam_role_policy" "scheduler" {
  count = length(local.schedules) > 0 ? 1 : 0
  name  = "invoke"
  role  = aws_iam_role.scheduler[0].id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = "lambda:InvokeFunction"
      Resource = [aws_lambda_function.this.arn, "${aws_lambda_function.this.arn}:*"]
    }]
  })
}

resource "aws_scheduler_schedule" "this" {
  for_each                     = local.schedules
  name                         = local.schedule_names[each.key]
  schedule_expression          = local.schedule_expressions[each.key]
  schedule_expression_timezone = each.value.timezone

  flexible_time_window {
    mode = "OFF"
  }

  target {
    arn      = local.invoke_arn
    role_arn = aws_iam_role.scheduler[0].arn
    input    = each.value.input == null ? null : jsonencode(each.value.input)
  }

  depends_on = [terraform_data.triggers, aws_iam_role_policy.scheduler]
}
