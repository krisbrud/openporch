# Acceptance matrix

Every scenario in the
[specification](../openspec/changes/initial-platform-orchestrator-spec/specs)
and what verifies it. A test in `crates/server/src/tests/acceptance.rs` fails
when a scenario is missing here or a test named here no longer exists.

| Status | Meaning |
| --- | --- |
| covered | An automated test that CI runs exercises the scenario's outcome |
| manual | Only a step run by hand verifies it: `just e2e-aws` ([e2e](e2e.md)), `just bench` ([benchmark](benchmark.md)) or a tagged release ([releasing](releasing.md)) |
| deferred | It belongs to a later milestone in [`tasks.md`](../openspec/changes/initial-platform-orchestrator-spec/tasks.md) |
| gap | Enabled in this release but not verified. "gap (partial)" means part of the outcome is verified |

How the tests run:

- `cargo test` runs the Rust tests. CI also runs the `#[ignore]` tests whose
  names start with `opentofu` (the "OpenTofu driver" job) and the account and
  artifact checks against Floci. `reference_capacity_benchmark` runs only
  through `just bench`.
- The provider's Go tests and its acceptance tests (`TestAcc*`) run in CI
  through `provider/scripts/acceptance.sh`.
- The example modules' tests run in CI through `examples/modules/test.sh`.
- `just e2e-aws` runs against a real AWS account by hand. It last passed on
  2026-10-17 at commit `1134dfc`.

The first release needs every enabled scenario covered or manual, and the
manual ones run for that release. Until then the open gaps are listed at the
end.

## active-resources

| Scenario | Status | Verified by |
|---|---|---|
| Stable across deployments | covered | `crates/server/src/tests/resources.rs::stable_across_deployments`, `crates/store/src/sqlite/tests.rs::guresid_is_allocated_once_and_kept_across_deployments`; state keyed by GUResID: `crates/drivers/src/opentofu/tests.rs::opentofu_runs_a_local_module_end_to_end` |
| Read outputs | covered | `crates/server/src/tests/resources.rs::read_outputs` |
| Dependency removed then restored | covered | `crates/server/src/tests/resources.rs::dependency_removed_then_restored` |
| Scheduled deletion | covered | `crates/server/src/tests/resources.rs::scheduled_deletion`, `crates/engine/src/planner/tests.rs::a_scheduled_resource_is_deleted_by_the_next_deployment_without_it` |
| Usage after a partial failure | covered | `crates/server/src/tests/resources.rs::usage_after_a_partial_failure` |
| Detach | covered | `crates/server/src/tests/resources.rs::detach`, `crates/engine/src/planner/tests.rs::a_detached_resource_is_not_provisioned_again`, `crates/store/src/sqlite_permits/tests.rs::a_detach_uses_its_permit_with_the_detach` |
| Database deletion fails | covered | `crates/engine/src/planner/tests/teardown.rs::a_failed_deletion_keeps_the_environment_for_a_retry`, `crates/engine/src/planner/tests.rs::a_failed_deletion_blocks_what_it_uses_until_retried` |
| Definition switches backend | covered | `crates/server/src/tests/resources.rs::definition_switches_driver`, `crates/server/src/tests/plans.rs::a_state_backend_move_is_refused_before_acceptance`, `crates/drivers/src/opentofu/tests.rs::a_changed_state_backend_is_refused` |

## api-access

| Scenario | Status | Verified by |
|---|---|---|
| Request for the configured organization | covered | `crates/server/src/tests/mod.rs::single_organization` |
| Request for an unknown organization | covered | `crates/server/src/tests/mod.rs::single_organization`, `crates/server/src/tests/mod.rs::other_organizations_are_hidden_before_routing` |
| Unauthenticated request | gap (partial) | `crates/server/src/tests/mod.rs::authentication_and_authorization_on_every_route`, `crates/server/src/tests/mod.rs::router_walk_every_route_declares_a_permission_and_requires_credentials` (partial: the 401 is asserted, but no test shows a credential-less mutation leaves state unchanged) |
| Forbidden request | covered | `crates/server/src/tests/mod.rs::authentication_and_authorization_on_every_route`, `crates/server/src/tests/acceptance.rs::every_route_refuses_members_without_its_permission` |
| Validation failure | covered | `crates/server/src/tests/mod.rs::uniform_errors_and_identifiers` |
| Duplicate ID | covered | `crates/server/src/tests/mod.rs::uniform_errors_and_identifiers` |
| Healthy server | covered | `crates/server/src/tests/mod.rs::health_and_version` |

## applications-and-environments

| Scenario | Status | Verified by |
|---|---|---|
| Create an application | covered | `crates/server/src/tests/envs.rs::application_starts_with_an_undeployed_development_environment`, `crates/server/src/tests/mod.rs::creator_becomes_owner` |
| Delete an application with provisioned resources | gap (partial) | `crates/engine/src/planner/tests/teardown.rs::deleting_an_application_deletes_every_environment_then_the_application`, `crates/engine/src/planner/tests/teardown.rs::adopted_and_external_resources_are_released_not_deleted`, `crates/server/src/tests/resources.rs::delete_a_deployed_application` (partial: no test asserts external or retained adopted objects are listed in the deletion result) |
| Delete a type in use | covered | `crates/server/src/tests/envs.rs::environment_types_in_use_are_kept` |
| Empty environment | gap (partial) | `crates/server/src/tests/envs.rs::environments_need_developer_and_deployer` (created env is `never_deployed`, no delta) (partial: no test lists a new environment's active resources and deployments to show both are empty) |
| Clone from a deployment | covered | `crates/server/src/tests/envs.rs::clone_an_environment_from_a_deployment` |
| Successful deletion | covered | `crates/engine/src/planner/tests/teardown.rs::successful_deletion_goes_in_reverse_dependency_order`, `crates/server/src/tests/resources.rs::delete_a_deployed_environment` (no namespace: Kubernetes is group 9) |
| Deprovision failure | covered | `crates/engine/src/planner/tests/teardown.rs::a_failed_deletion_keeps_the_environment_for_a_retry` |
| After a deployment | covered | `crates/server/src/tests/envs.rs::clone_an_environment_from_a_deployment`, `crates/server/src/tests/deploys.rs::deploy_a_delta` |

## audit-log

| Scenario | Status | Verified by |
|---|---|---|
| Deployment recorded | covered | `crates/server/src/tests/deploys.rs::deployments_are_recorded_with_their_correlated_outcome` |
| Filter deletions | covered | `crates/server/src/tests/mod.rs::audit_query_filters_and_pages` |
| Delete attempt | covered | `crates/server/src/tests/mod.rs::audit_access_and_deletion` |
| Expire past retention | gap (partial) | `crates/server/src/tests/mod.rs::audit_entries_expire_only_when_an_administrator_expires_them` (partial: no test shows the expiry's audit record shares the deletion's transaction) |
| Accepted deployment later fails | covered | `crates/server/src/tests/deploys.rs::deployments_are_recorded_with_their_correlated_outcome`, `crates/server/src/tests/resources.rs::a_failed_operation_is_correlated_with_its_request` |
| Audit storage unavailable | covered | `crates/server/src/tests/mod.rs::audit_failure_rejects_the_mutation`, `crates/store/src/sqlite/tests.rs::failed_audit_write_rejects_the_mutation` |

## authentication

| Scenario | Status | Verified by |
|---|---|---|
| Actor recorded | gap | No test asserts a definition's `created_by` is the creating service user. Searched `created_by` across `crates/*/src/tests`: only sets, value versions and grants assert it (as `bootstrap`); `DefinitionRecord` serializes `created_by` but no catalog test reads it |
| Token shown once | gap (partial) | `crates/server/src/tests/mod.rs::tokens_are_shown_once_and_revocable` (partial: later reads are not checked for the token's ID, description, expiry and creation time) |
| Revoked token | covered | `crates/server/src/tests/mod.rs::tokens_are_shown_once_and_revocable` |
| Who may create tokens | gap (partial) | `crates/server/src/tests/mod.rs::who_may_create_tokens`, `crates/server/src/tests/mod.rs::manager_delegation_ceiling` (partial: the cited tests do not cover an administrator creating a service user token or the delegation ceiling; see also `crates/server/src/tests/mod.rs::tokens_are_shown_once_and_revocable`, `crates/server/src/tests/mod.rs::service_user_beyond_the_issuers_authority`) |
| Missing token | gap (partial) | `crates/server/src/tests/mod.rs::missing_bearer_header` (partial: only the 401 on a GET is asserted; no test shows the request is not processed) |
| Login and expiry | gap (partial) | `crates/server/src/tests/mod.rs::sessions_expire_after_24_hours`; CLI hint: `crates/cli/src/tests.rs::login_with_a_token_stores_it_owner_only` (asserts "porch login" only when no credential is stored; the 401-to-relogin hint for an expired session is not asserted) |
| GitHub device login | covered | `crates/server/src/tests/github.rs::device_login_maps_the_immutable_github_id`, `crates/cli/src/tests.rs::device_login_stores_a_session_for_the_github_user` |
| Unknown identity | covered | `crates/server/src/tests/mod.rs::unknown_identities_need_an_invitation` |
| Provider is locked after first login | gap (partial) | `crates/server/src/tests/mod.rs::identity_binding_is_locked_after_first_login` (login layer with an OIDC-shaped identity; generic OIDC login itself is task 13.1) (partial: no test shows an administrator changing the identity binding, or that only an administrator can) |
| First start | covered | `crates/server/src/tests/mod.rs::first_start_bootstraps_an_administrator`, `crates/server/tests/bootstrap_logs.rs::bootstrap_token_is_not_logged` |
| Reused GitHub name | covered | `crates/server/src/tests/github.rs::renamed_and_reused_github_logins`, `crates/server/src/tests/mod.rs::reused_github_login_gains_nothing` |
| Unverified invitation email | covered | `crates/server/src/tests/mod.rs::unverified_invitation_email_is_denied`, `crates/server/src/tests/github.rs::email_invitations_need_a_verified_github_email` |
| Removed user with a valid signature | covered | `crates/server/src/tests/mod.rs::sessions_check_membership_and_revocation_on_every_request` |
| Invalid server certificate | gap (partial) | CLI half covered: `crates/client/src/tests.rs::untrusted_certificate_is_rejected_before_the_token_is_sent`. Provider half untested: searched `provider/internal` for `tls`, `certificate`, `httptest.NewTLSServer`; only `TestPlainHTTPIsRefusedOffLoopback` / `TestServerURLsNeedHTTPSExceptOnLoopback` exist |
| Wrong audience or replayed transaction | deferred | Generic OIDC login, task 13.1 |

## aws-lambda-workloads

| Scenario | Status | Verified by |
|---|---|---|
| Zip function | covered | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::zip_function`; real function: manual `just e2e-aws` |
| Image function | covered | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::image_from_ecr`, `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::image_function_without_image` |
| Container workload on Lambda | gap (partial) | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::container_with_web_adapter_port`, `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::two_containers`, `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::container_files`; real HTTP invocation is of the image function `examples/lambda-api` in `just e2e-aws`, not of a container workload with the adapter |
| Non-ECR image | gap (partial) | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::image_outside_ecr`, `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::image_in_another_region` (partial: `expect_failures` does not assert the message that images must come from ECR in the same region) |
| Database password | covered | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::secret_variables_are_references`; `just e2e-aws` checks `SIGNING_KEY_SECRET_REF` in the real function |
| Function URL | manual | `just e2e-aws` step "Call the API's URL"; config only: `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::container_with_web_adapter_port`, `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::iam_url`, `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::inferred_url_is_not_public` |
| Queue trigger | covered | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::queue_trigger`; real mapping and message processing: `just e2e-aws` |
| Nightly schedule | covered | `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::schedules`; `just e2e-aws` checks `cron(0 3 * * ? *) Europe/Oslo` |
| DynamoDB access | manual | `just e2e-aws` step "Send a signed order and see the worker store it"; supporting: `examples/modules/aws-lambda-workload/tests/unit.tftest.hcl::zip_function` (policy attachment), `examples/modules/dynamodb-table/policy/tests/policy.tftest.hcl::read_write`, `crates/engine/src/planner/tests/coprovision.rs::a_policy_is_in_place_before_the_workload_that_needs_it` |
| Table from a workload file | covered | `examples/modules/dynamodb-table/tests/table.tftest.hcl::default_key`, `crates/drivers/src/opentofu/tests.rs::opentofu_modules_read_the_resource_inputs`, `crates/server/src/tests/e2e.rs::the_example_lambda_workloads_deploy_and_replay` |
| Rollback to a previous artifact | manual | `just e2e-aws` step "Replay the first deployment's snapshot" (by deployment ID, not `+1`); supporting: `crates/server/src/tests/deploys.rs::redeploys_replay_the_recorded_snapshot_unless_asked_for_current` (`+1`), `crates/server/src/tests/e2e.rs::the_example_lambda_workloads_deploy_and_replay` (artifact version replay) |
| Delete environment | manual | `just e2e-aws` steps "Delete the environment with a permit for each deletion" and "Check that nothing the environment owned is left" |
| Function uses a secret | manual | `just e2e-aws` step "Check that the signing key is in no log, output or function environment"; `examples/lambda-order-worker/app/test_app.py` |
| Workload removed | gap (partial) | Engine ordering covered by `crates/engine/src/planner/tests.rs::a_removed_workload_keeps_its_unscheduled_dependencies`. Removing one Lambda workload while its queue stays is not run against AWS or Floci: `scripts/e2e-aws.sh` only deletes the whole environment, and `aws-lambda-workload` module tests only plan |

## cli

| Scenario | Status | Verified by |
|---|---|---|
| Environment variables | covered | `crates/cli/src/tests.rs::environment_variables_list_the_organizations_applications` |
| Deploy and wait | covered | `crates/cli/src/tests/deploys.rs::deploy_and_wait` |
| Score alias | covered | `crates/cli/src/tests/deploys.rs::score_alias_redeploys_and_diffs` |
| Apply a definition file | covered | `crates/cli/src/tests/objects.rs::apply_a_definition_file` |
| Follow logs | covered | `crates/cli/src/tests/deploys.rs::follow_logs_of_a_running_deployment` |
| Create CI token | covered | `crates/cli/src/tests/objects.rs::create_ci_token` |
| Raw get | covered | `crates/cli/src/tests/deploys.rs::raw_api_requests` |
| Stale confirmation | covered | `crates/cli/src/tests/plans.rs::a_stale_preview_is_refused_and_deletes_nothing` |
| Interrupted deployment | covered | `crates/cli/src/tests/plans.rs::an_interrupted_deployment_names_the_resource_and_its_recovery` |
| Response lost after acceptance | covered | `crates/cli/src/tests/deploys.rs::lost_acceptance_response_returns_the_original_deployment` |

## cloud-accounts

| Scenario | Status | Verified by |
|---|---|---|
| Create a static account | covered | `crates/server/src/tests/accounts.rs::create_a_static_account` |
| Create a role account | covered | `crates/server/src/tests/accounts.rs::create_a_role_account`, `crates/engine/src/cloud/tests.rs::role_accounts_assume_the_role_with_the_external_id` |
| Wrong external ID | covered | `crates/server/src/tests/accounts.rs::account_check_returns_the_caller_identity` (mocked STS); `crates/engine/src/cloud/tests.rs::real_account_check` (ignored; CI against Floci, once against real AWS) |
| OpenTofu driver with a role account | gap (partial) | `crates/engine/src/cloud/tests.rs::drivers_receive_only_the_account_credentials`, `crates/drivers/src/opentofu/tests.rs::fake_run_passes_credentials_through_files_and_environment_only` (partial: no test runs an OpenTofu definition with `driver_account` through the executor and checks the driver log for credentials) |
| Delete in use | covered | `crates/server/src/tests/accounts.rs::delete_an_account_in_use` |
| Expiring role session | covered | `crates/engine/src/cloud/tests.rs::role_sessions_are_reused_and_refreshed_before_expiry`, `crates/engine/src/cloud/tests.rs::work_longer_than_a_session_fails_before_any_call` |

## control-plane-operations

| Scenario | Status | Verified by |
|---|---|---|
| Small installation | covered | `crates/cli/src/tests/quickstart.rs::the_quickstart_runs_as_written` |
| Second process | covered | `crates/server/src/tests/keys.rs::a_second_server_on_the_same_installation_exits_before_any_work`, `crates/store/src/sqlite/tests.rs::second_owner_is_refused` |
| Database works but key is missing | gap (partial) | `crates/server/src/tests/mod.rs::missing_master_key_blocks_readiness_and_mutations` (partial: the refusal is shown for creating an application, not for running a deployment) |
| Restore after total server loss | covered | `crates/server/src/tests/restore.rs::backup_and_restore_drill` |
| Unsupported downgrade | covered | `crates/server/src/tests/upgrades.rs::an_older_build_refuses_every_newer_fixture_unchanged`, `crates/store/src/sqlite/tests.rs::newer_schema_is_refused_without_changes` |
| Uninstall with active resources | gap (partial) | `crates/server/src/tests/e2e.rs::stopping_and_restarting_the_server_deletes_nothing`, `crates/server/src/tests/restore.rs::backup_and_restore_drill` (instructions in `docs/operations.md`) (partial: no test performs the uninstall described in docs/operations.md) |
| Concurrent environments | covered | `crates/engine/src/planner/tests.rs::concurrency_is_bounded_across_environments`, `crates/server/src/tests/deploys.rs::running_deployments_are_bounded_and_status_stays_available` |
| Storage exhausted | gap (partial) | `crates/engine/src/planner/tests.rs::the_log_stops_at_its_quota_but_keeps_the_outcome`, `crates/engine/src/planner/tests.rs::lost_log_lines_fail_the_deployment`, `crates/server/src/tests/mod.rs::audit_failure_rejects_the_mutation` (partial: rejection is shown only when the audit write fails, not when operation records cannot be stored) |
| Benchmark under driver load | manual | `just bench` runs the ignored `crates/server/src/tests/benchmark.rs::reference_capacity_benchmark` (asserts p95 thresholds); not in CI; results in `docs/benchmark.md` |
| Diagnose a timeout | covered | `crates/server/src/tests/resources.rs::diagnostics_correlate_a_timed_out_deployment_without_secrets` |
| Release from clean checkout | manual | `.github/workflows/release.yml` on a `v*` tag (PRs touching the release build run its locked-input, test, fixture and checksum steps without attestation); task 14.5 is still open, so no tagged run is recorded |
| Incompatible previous binary | covered | `crates/server/src/tests/keys.rs::an_older_binary_refuses_a_newer_schema_and_names_the_way_out` |
| Clean release build | manual | `.github/workflows/release.yml` + `scripts/release-archive.sh`, `docs/releasing.md` "Verifying a release"; not yet run on a tag (14.5 open) |
| Contributor follows release guidance | manual | `docs/releasing.md` "Release checklist", `CONTRIBUTING.md`, `SECURITY.md` (documentation review; 14.5 open) |

## deployment-safety

| Scenario | Status | Verified by |
|---|---|---|
| Unsupported target | deferred | Kubernetes workloads, group 9 |
| Database replacement | gap (partial) | `crates/engine/src/executor/tests.rs::a_replacement_needs_a_permit_for_its_plan`, `crates/server/src/tests/plans.rs::a_refused_driver_plan_shows_in_the_next_preview` (partial: `approval_required` is checked, but not the preview the record should carry) |
| Inputs become resolvable later | gap (partial) | `crates/engine/src/executor/tests.rs::a_replacement_needs_a_permit_for_its_plan`, `crates/server/src/tests/plans.rs::a_refused_driver_plan_shows_in_the_next_preview` (partial: no test has earlier dependencies finish before the late driver plan and asserts they stay tracked and the deployment fails) |
| Protected environment | covered | `crates/server/src/tests/plans.rs::destructive_permits_in_a_protected_environment_need_an_administrator` |
| Pre-existing namespace | deferred | Kubernetes namespaces, group 9 |
| Developer requests host access | deferred | Kubernetes target fragments, group 9 (task 9.2) |
| File escapes the run directory | covered | `crates/drivers/src/opentofu/tests.rs::inputs_are_checked_before_anything_runs`, `crates/drivers/src/opentofu/tests.rs::symlinks_cannot_lead_out_of_a_git_module` |
| Dependency output reveals replacement | gap | No test binds an `unknown`-impact permit and then has the driver plan a replacement. Searched `Unknown`/`unknown` and `Replace` in `crates/store/src/sqlite_permits/tests.rs`, `crates/engine/src/executor/tests.rs`, `crates/server/src/tests/plans.rs`; `a_bound_permit_authorizes_its_deployment_and_retries` covers only a different fingerprint |
| Generation increments after acceptance | covered | `crates/store/src/sqlite_permits/tests.rs::a_bound_permit_authorizes_its_deployment_and_retries` |
| External driver redirects credentials | gap | `crates/drivers/src/http.rs` sets `redirect::Policy::none()`, but no test serves a 3xx. Searched `redirect`, `30[1278]`, `Location` in `crates/drivers/src/http_tests.rs` and `crates/drivers/fixtures/http/*.json` (`TestRedirectsAreNotFollowed` covers the Go provider's client, not drivers) |

## deployment-sets-and-deltas

| Scenario | Status | Verified by |
|---|---|---|
| Identical content, identical ID | covered | `crates/server/src/tests/sets.rs::identical_content_identical_id`, `crates/core/src/sets.rs::identical_content_identical_id`, `crates/server/src/tests/e2e.rs::a_function_with_a_database_and_a_queue_deploys_and_replays` |
| Patch a variable | covered | `crates/server/src/tests/sets.rs::patch_a_variable`, `crates/core/src/sets.rs::patch_a_variable` |
| Modify a deployed delta | covered | `crates/server/src/tests/sets.rs::deltas_are_mutable_until_deployed` |
| Add to an empty environment | covered | `crates/server/src/tests/sets.rs::add_to_an_empty_environment`, `crates/core/src/sets.rs::add_to_empty_environment` |
| Redeploy unchanged workload | covered | `crates/cli/src/tests/deploys.rs::score_alias_redeploys_and_diffs`, `crates/core/src/sets.rs::workload_delta_adds_updates_or_changes_nothing` |
| Diff last two deployments | covered | `crates/cli/src/tests/deploys.rs::score_alias_redeploys_and_diffs` |

## deployments

| Scenario | Status | Verified by |
|---|---|---|
| Deploy a delta | covered | `crates/server/src/tests/deploys.rs::deploy_a_delta` |
| Stale or concurrent request | gap (partial) | `crates/server/src/tests/deploys.rs::stale_or_concurrent_requests_conflict`, `crates/store/src/sqlite/tests.rs::stale_generation_and_running_deployments_conflict_without_changes` (partial: no test has an environment deletion still running when the request arrives) |
| Lost HTTP response | covered | `crates/server/src/tests/deploys.rs::a_lost_response_is_retried_with_the_same_key`, `crates/store/src/sqlite/tests.rs::idempotent_retry_returns_the_original_deployment` |
| Missing deployer role | covered | `crates/server/src/tests/deploys.rs::deploying_needs_the_deployer_role` |
| Partial success | gap (partial) | `crates/server/src/tests/resources.rs::usage_after_a_partial_failure` (database result and last success kept), `crates/server/src/tests/envs.rs::clone_an_environment_from_a_deployment` (`partial` convergence after a failure), `crates/engine/src/planner/tests.rs::one_branch_fails` (partial: no test has the database succeed while the workload fails; partial convergence is asserted only on a constructed record) |
| One branch fails | covered | `crates/engine/src/planner/tests.rs::one_branch_fails` |
| Permanent driver error | covered | `crates/engine/src/planner/tests.rs::permanent_errors_are_not_retried`, `crates/drivers/src/http_tests.rs::protocol_fixtures` (`put-400-validation.json`) |
| Crash after creation | covered | `crates/engine/src/planner/tests/recovery.rs::a_crash_after_creation_is_recovered_by_observation`, `crates/engine/src/planner/tests/recovery.rs::nothing_found_is_provisioned_again_with_the_same_identity` |
| Driver keeps running after timeout | covered | `crates/engine/src/planner/tests.rs::a_timeout_leaves_the_resource_unknown_until_recovered`, `crates/engine/src/planner/tests/recovery.rs::an_operation_still_running_is_left_unknown` |
| Configuration changes during execution | gap (partial) | `crates/server/src/tests/e2e.rs::a_function_with_a_database_and_a_queue_deploys_and_replays`, `crates/server/src/tests/deploys.rs::redeploys_replay_the_recorded_snapshot_unless_asked_for_current` (snapshot vs current; no test edits configuration while a run is in flight) |
| Unchanged deployment | covered | `crates/server/src/tests/resources.rs::stable_across_deployments`, `crates/server/src/tests/e2e.rs::a_function_with_a_database_and_a_queue_deploys_and_replays`, `crates/drivers/src/opentofu/tests.rs::opentofu_runs_a_local_module_end_to_end` (second plan has no effects) |
| Recover after a failed deployment | gap (partial) | `crates/cli/src/tests/deploys.rs::logs_replays_and_stored_deltas`, `crates/server/src/tests/deploys.rs::redeploys_replay_the_recorded_snapshot_unless_asked_for_current` (partial: no failed deployment precedes the replay, and current permissions or permit policy on the replay are not checked) |
| Artifact no longer exists | covered | `crates/engine/src/planner/tests/workloads.rs::a_missing_artifact_stops_the_deployment_before_anything_runs`, `crates/server/src/tests/e2e.rs::a_deployment_pins_its_versions_and_replay_refuses_missing_inputs` |
| Redacted failure | covered | `crates/engine/src/executor/tests.rs::opentofu_failure_is_redacted_in_the_deployment_log` (ignored, runs in CI), `crates/server/src/tests/resources.rs::read_a_deployments_redacted_log` |
| Retry after generation advanced | covered | `crates/server/src/tests/deploys.rs::a_lost_response_is_retried_with_the_same_key`, `crates/cli/src/tests/deploys.rs::lost_acceptance_response_returns_the_original_deployment` |
| Reading after partial failure | gap (partial) | `crates/server/src/tests/resources.rs::inspect_the_graph_after_a_failed_deployment`, `crates/server/src/tests/envs.rs::clone_an_environment_from_a_deployment` (partial: partial convergence is asserted only on a constructed failed record) |

## drivers

| Scenario | Status | Verified by |
|---|---|---|
| Asynchronous provisioning | covered | `crates/drivers/src/tests.rs::asynchronous_provisioning_polls_get_until_ready`, `crates/drivers/src/http_tests.rs::asynchronous_provisioning_over_http` |
| Cookie round trip | covered | `crates/drivers/src/tests.rs::cookie_round_trip`, `crates/engine/src/executor/tests.rs::the_driver_cookie_survives_between_deployments_sealed` |
| Register an external driver | gap (partial) | Registration and definition use: `crates/server/src/tests/catalog.rs::external_drivers_are_registered`; HTTP calls to a target: `crates/drivers/src/http_tests.rs`. But "provisioning calls that URL" is never exercised and appears unimplemented: `crates/server/src/state.rs` runs deployments with `DriverRegistry::with_builtins().with_opentofu(..)` and nothing in `crates/server` or `crates/engine` constructs an `HttpDriver` from a registered driver |
| Static database instance | covered | `crates/drivers/src/tests.rs::echo_returns_driver_inputs` |
| Workload without a runtime | covered | `crates/drivers/src/tests.rs::none_returns_resolved_resource_inputs`, `crates/engine/src/planner/tests/workloads.rs::a_none_workload_stores_its_resolved_document` |
| Deterministic password | covered | `crates/drivers/src/tests.rs::template_keeps_a_generated_password_in_its_cookie` |
| Driver error | covered | `crates/drivers/src/http_tests.rs::protocol_fixtures` (`put-400-validation.json`), `crates/engine/src/planner/tests.rs::permanent_errors_are_not_retried` |
| Hung driver | covered | `crates/drivers/src/tests.rs::a_driver_that_does_not_answer_times_out`, `crates/drivers/src/tests.rs::a_timeout_cancels_drivers_that_can`, `crates/engine/src/planner/tests.rs::a_timeout_leaves_the_resource_unknown_until_recovered` |
| Incompatible cookie upgrade | gap (partial) | Per-call checks only: `crates/drivers/src/opentofu/tests.rs::cookies_are_checked_for_compatibility`, `crates/drivers/src/tests.rs::returned_cookies_are_checked`. No driver-upgrade activation step that rejects with affected GUResIDs; searched `activat`, `cookie_version`, `upgrade` across `crates/` |
| Duplicate put | covered | `crates/drivers/src/http_tests.rs::duplicate_put_recovers_the_same_object` |
| Unobservable timed-out creation | covered | `crates/engine/src/planner/tests/recovery.rs::nothing_found_by_a_driver_that_cannot_recover_waits_for_an_administrator`, `crates/engine/src/planner/tests/recovery.rs::an_unobservable_outcome_waits_for_an_administrator` |
| Conflict after a lost response | gap (partial) | 409 mapped to a non-retryable conflict: `crates/drivers/src/http_tests.rs::protocol_fixtures` (`put-409-conflict.json`), `crates/engine/src/executor/tests.rs::a_failure_after_the_driver_accepted_a_change_is_uncertain`. The executor maps `DriverError::Conflict` to a permanent failure (`crates/engine/src/executor.rs` `driver_failure`); no test shows the engine observing the prior operation after the 409 |

## kubernetes-workloads

| Scenario | Status | Verified by |
|---|---|---|
| Cluster reference | deferred | Group 9 |
| EKS cluster | deferred | Group 9 |
| Namespace per environment | deferred | Group 9 |
| Function workload on Kubernetes | deferred | Group 9 |
| Scheduled container | deferred | Group 9 |
| Render a web workload | deferred | Group 9 |
| Service account injection | deferred | Group 9 |
| Crash-looping container | deferred | Group 9 |
| Remove a workload | deferred | Group 9 |
| Read status | deferred | Group 9 |
| Shared service account | deferred | Group 9 |
| Field conflict | deferred | Group 9 |

## oidc-provider

| Scenario | Status | Verified by |
|---|---|---|
| Discovery | deferred | Group 11 |
| Key rotation | deferred | Group 11 |
| Role assumption without standing credentials | deferred | Group 11 |
| Trust scoped to another account | deferred | Group 11 |
| Print trust policy | deferred | Group 11 |
| No public URL | deferred | Group 11; `aws-identity` is reported unavailable by `crates/server/src/tests/accounts.rs::unavailable_account_types_name_the_missing_capability` |

## opentofu-driver

| Scenario | Status | Verified by |
|---|---|---|
| Inline script | covered | `crates/drivers/src/opentofu/tests.rs::opentofu_runs_a_local_module_end_to_end` (inline `script`, local providers); S3 bucket module against Floci: `examples/modules/s3/tests/bucket.tftest.hcl` |
| Neither source nor script | covered | `crates/server/src/tests/catalog.rs::opentofu_definition_inputs_are_checked_when_written`, `crates/drivers/src/opentofu/tests.rs::inputs_are_checked_before_anything_runs` |
| Table key from params | covered | `crates/drivers/src/opentofu/tests.rs::opentofu_modules_read_the_resource_inputs`, `examples/modules/dynamodb-table/tests/table.tftest.hcl::key_sort_key_and_index` |
| Secret workload variable | covered | `crates/drivers/src/opentofu/tests.rs::secret_resource_inputs_are_moved_out_one_by_one`, `crates/drivers/src/opentofu/tests.rs::a_secret_inside_another_moves_with_it`, `crates/drivers/src/opentofu/tests.rs::the_plain_variable_file_has_no_secret_input` |
| Reserved variable name | covered | `crates/server/src/tests/catalog.rs::opentofu_definition_inputs_are_checked_when_written` |
| Git module with variables | gap (partial) | `crates/drivers/src/opentofu/tests.rs::opentofu_applies_a_git_module_and_deletes_the_retained_commit` (git module with variables; the RDS module itself is milestone 2) (partial: the module returns `length`, not host, port, name, username and password) |
| Default AWS environment | covered | `crates/drivers/src/opentofu/tests.rs::fake_run_exports_the_default_aws_environment`; real: `just e2e-aws` |
| Orchestrator-managed state | covered | `crates/drivers/src/opentofu/tests.rs::opentofu_s3_state_applies_twice` (CI against Floci S3; once against real S3) |
| Sensitive output | covered | `crates/drivers/src/opentofu/tests.rs::opentofu_runs_a_local_module_end_to_end` |
| Environment deleted | covered | Driver destroy and state removal: `crates/drivers/src/opentofu/tests.rs::opentofu_runs_a_local_module_end_to_end`, `crates/drivers/src/opentofu/tests.rs::opentofu_s3_state_applies_twice`; environment teardown: `crates/engine/src/planner/tests/teardown.rs::successful_deletion_goes_in_reverse_dependency_order`; real bucket: `just e2e-aws` |
| Apply fails | gap (partial) | `crates/drivers/src/opentofu/tests.rs::opentofu_failures_are_reported_redacted`, `crates/engine/src/executor/tests.rs::opentofu_failure_is_redacted_in_the_deployment_log` (generic failure, not an AWS IAM error) |
| Partial apply writes state | gap (partial) | `crates/drivers/src/opentofu/tests.rs::a_failed_apply_is_uncertain_and_keeps_what_it_ran`, `crates/drivers/src/opentofu/tests.rs::opentofu_a_timed_out_apply_leaves_usable_state` (partial: no test has an apply create one object, fail on a second, then retry) |
| Missing state | covered | `crates/drivers/src/opentofu/tests.rs::lost_state_needs_recovery`, `crates/drivers/src/opentofu/tests.rs::state_lost_after_an_apply_started_is_never_replaced` |
| Module revision removed upstream | covered | `crates/engine/src/planner/tests.rs::a_removal_whose_retained_version_is_gone_changes_nothing`, `crates/drivers/src/opentofu/tests.rs::opentofu_applies_a_git_module_and_deletes_the_retained_commit`, `crates/drivers/src/opentofu/tests.rs::deleting_a_script_resource_never_fetches_a_newer_source` |
| State and resource accounts differ | covered | `crates/drivers/src/opentofu/tests.rs::s3_state_uses_a_workspace_per_resource_and_its_own_credentials` |

## opentofu-provider

| Scenario | Status | Verified by |
|---|---|---|
| Configure from environment | covered | `provider/internal/provider/provider_test.go::TestConfigureFromEnvironment`, `provider/scripts/smoke.sh` |
| Secret store | deferred | `openporch_secret_store` and `aws-ssm` adapters are tasks 13.3 / 13.2 |
| Definition with criteria | covered | `provider/internal/provider/acc_definition_test.go::TestAccResourceDefinitionWithCriteria` |
| Import | covered | `provider/internal/provider/acc_test.go::TestAccImportKeepsTheName`, `provider/internal/provider/acc_test.go::TestAccApplicationAndEnvironment` |
| Placeholder escaping | covered | `provider/internal/provider/acc_definition_test.go::TestAccResourceDefinitionWithCriteria` |
| Secret value changed | covered | `provider/internal/provider/acc_catalog_test.go::TestAccValue` |
| Service user token | covered | `provider/internal/provider/acc_identity_test.go::TestAccServiceUserTokenAndRoles` |
| Reviewer looks for a deploy resource | gap | Documented only (`provider/internal/provider/provider.go` description, `provider/docs/index.md`); no test asserts the schema has no deployment resource. Searched `provider/internal/**/*_test.go` for `Resources()`, `deploy` |
| Accidental application removal from HCL | covered | `provider/internal/provider/acc_test.go::TestAccApplicationAndEnvironment` (`destroy_acknowledgements`, `ExpectError`) |

## organization-membership

| Scenario | Status | Verified by |
|---|---|---|
| Last administrator | covered | `crates/server/src/tests/mod.rs::last_administrator_is_kept` |
| Accept by GitHub login | covered | `crates/server/src/tests/mod.rs::accept_invitation_by_github_identity`, `crates/server/src/tests/github.rs::invitations_by_login_resolve_through_github` |
| Expired invitation | covered | `crates/server/src/tests/mod.rs::expired_invitation` |
| Manager invites an administrator | covered | `crates/server/src/tests/mod.rs::manager_delegation_ceiling` |
| CI service user | covered | `crates/server/src/tests/mod.rs::ci_service_user` |
| Removal revokes access | covered | `crates/server/src/tests/mod.rs::removal_revokes_access` |
| Manager removes an administrator | covered | `crates/server/src/tests/mod.rs::manager_delegation_ceiling`, `crates/server/src/tests/mod.rs::managers_remove_any_non_administrator` |
| Manager targets bootstrap | covered | `crates/server/src/tests/mod.rs::manager_delegation_ceiling` |
| Manager creates or promotes an administrator | covered | `crates/server/src/tests/mod.rs::manager_delegation_ceiling` |
| Service user exceeds issuer's authority | covered | `crates/server/src/tests/mod.rs::service_user_beyond_the_issuers_authority` |

## placeholders

| Scenario | Status | Verified by |
|---|---|---|
| Composite string | covered | `crates/core/src/placeholder.rs::composite_string` |
| Unknown root | covered | `crates/core/src/placeholder.rs::unknown_root_names_placeholder_and_location`, `crates/core/src/graph/tests.rs::unknown_types_and_bad_placeholders_fail` |
| Namespace from context | deferred | `k8s-namespace` is group 9 (context composition itself: `crates/core/src/placeholder.rs::composite_string`, `crates/core/src/placeholder.rs::context_keys`) |
| Cloud account from a config resource | covered | `crates/core/src/placeholder.rs::guresid_and_account`, `crates/engine/src/planner/tests.rs::independent_branches_run_in_parallel` (route's `driver_account` from `${resources.dns.account}`) |
| Whole-value object | covered | `crates/core/src/placeholder.rs::whole_value_keeps_json_type` |
| Secret in a value input | covered | `crates/core/src/placeholder.rs::secret_in_value_input_fails` |
| Shared DNS host in a variable | covered | `crates/core/src/placeholder.rs::shared_dns_host_in_a_variable` |
| Composite connection string | covered | `crates/core/src/placeholder.rs::composite_secret_is_kept_unevaluated`, `crates/engine/src/executor/tests.rs::a_secret_in_a_composite_string_is_rendered_only_for_the_driver` |

## rbac

| Scenario | Status | Verified by |
|---|---|---|
| Union of roles | covered | `crates/server/src/tests/mod.rs::union_of_roles_and_hidden_applications`, `crates/server/src/rbac.rs::union_of_roles` |
| Manager creates a definition | covered | `crates/server/src/tests/catalog.rs::definition_over_the_api` |
| Developer edits definitions | covered | `crates/server/src/tests/catalog.rs::definition_over_the_api` |
| Viewer deploys | covered | `crates/server/src/tests/deploys.rs::deploying_needs_the_deployer_role` |
| Org viewer reads secrets | covered | `crates/server/src/tests/catalog.rs::definition_secrets_are_stored_as_references` |
| Developer without deployer | covered | `crates/server/src/tests/deploys.rs::deploying_needs_the_deployer_role`, `crates/server/src/rbac.rs::deployer_gate_names_the_missing_role` |
| Create an application | covered | `crates/server/src/tests/mod.rs::creator_becomes_owner` |
| Hidden application | gap (partial) | `crates/server/src/tests/mod.rs::hidden_and_missing_applications_are_404_and_mutations_are_audited` (partial: the cited test exercises a DELETE, not a read; see also `crates/server/src/tests/mod.rs::union_of_roles_and_hidden_applications`) |
| Known application, forbidden action | covered | `crates/server/src/tests/mod.rs::authentication_and_authorization_on_every_route` |
| Grant through the provider | covered | `provider/internal/provider/acc_identity_test.go::TestAccServiceUserTokenAndRoles` |
| Cross-application deployment reference | covered | `crates/server/src/tests/deploys.rs::cross_application_references_are_hidden` |
| Development user changes production configuration | covered | `crates/server/src/tests/values.rs::development_user_changes_production_configuration` |

## resource-catalog

| Scenario | Status | Verified by |
|---|---|---|
| List types | covered | `crates/server/src/tests/catalog.rs::built_in_types_are_listed_with_schemas`, `crates/core/src/catalog.rs::builtin_type_list` |
| Workload type | covered | `crates/core/src/catalog.rs::workload_type` |
| Postgres outputs | covered | `crates/core/src/catalog.rs::postgres_schema` |
| Create a custom type | gap (partial) | `crates/server/src/tests/catalog.rs::custom_types_are_org_prefixed_and_built_ins_are_fixed`; workload request of a custom type: `crates/core/src/graph/tests.rs::unknown_types_and_bad_placeholders_fail` (partial: no test has a workload request the created type and build it into a graph) |
| Modify a built-in type | covered | `crates/server/src/tests/catalog.rs::custom_types_are_org_prefixed_and_built_ins_are_fixed` |
| Request an undefined class | covered | `crates/core/src/graph/tests.rs::undefined_class_fails_naming_it` |
| Missing required output | gap | Only the pure check is tested (`crates/core/src/catalog.rs::output_validation`). `ResourceType::validate_outputs` / `validate_secret_output` are never called outside `crates/core/src/catalog.rs`, so provisioning does not enforce output schemas; searched `validate_outputs`, `output_kind`, `missing output` in `crates/engine`, `crates/server` |
| Catalog schema fixture | covered | `crates/core/src/catalog.rs::published_catalog_fixtures_are_current`, `crates/core/src/catalog.rs::catalog_output_conformance` |

## resource-definitions

| Scenario | Status | Verified by |
|---|---|---|
| Create a definition over the API | covered | `crates/server/src/tests/catalog.rs::definition_over_the_api` |
| Unknown driver | covered | `crates/server/src/tests/catalog.rs::definition_over_the_api` |
| Read a definition with secrets | covered | `crates/server/src/tests/catalog.rs::definition_secrets_are_stored_as_references` |
| External reference | deferred | External store adapters, task 13.2 |
| More specific criterion wins | covered | `crates/core/src/criteria.rs::more_specific_criterion_wins` |
| No match | covered | `crates/core/src/criteria.rs::no_match_names_type_class_id_and_context` |
| Ambiguous match | covered | `crates/core/src/criteria.rs::ties_are_ambiguous_and_name_both` |
| Add a criterion | covered | `crates/server/src/tests/catalog.rs::criteria_are_managed_one_by_one` |
| Policy co-provisioned with a database | covered | `crates/core/src/graph/tests.rs::policy_coprovisioned_with_a_database`, `crates/engine/src/planner/tests/coprovision.rs::a_policy_is_in_place_before_the_workload_that_needs_it` |
| Delete in use | covered | `crates/server/src/tests/catalog.rs::definition_in_use_is_kept` |
| Policy edge required by workload | covered | `crates/engine/src/planner/tests/coprovision.rs::a_policy_is_in_place_before_the_workload_that_needs_it`, `crates/engine/src/planner/tests/coprovision.rs::the_order_workloads_are_added_in_changes_nothing`, `crates/core/src/graph/tests.rs::graph_is_independent_of_definition_order` |

## resource-graph

| Scenario | Status | Verified by |
|---|---|---|
| Workload with a database | covered | `crates/core/src/graph/tests.rs::workload_with_a_database` |
| Kubernetes workload definition adds cluster nodes | deferred | Group 9 (a graph-level test already exists: `crates/core/src/graph/tests.rs::kubernetes_workload_definition_adds_cluster_nodes`) |
| Cycle | covered | `crates/core/src/graph/tests.rs::cycle_is_reported` |
| Shared DNS per environment | gap (partial) | Per-environment identity is shown only for a private database (`crates/server/src/tests/e2e.rs::a_function_with_a_database_and_a_queue_deploys_and_replays`, `assert_ne!` on GUResIDs). No test deploys a `shared.*` resource to two environments and compares records/outputs; searched `shared` in `crates/server/src/tests/e2e.rs` and `crates/server/src/tests/resources.rs`, `crates/engine/src/planner/tests*` |
| Inherit ID | covered | `crates/core/src/graph/tests.rs::workload_with_a_database`, `crates/core/src/placeholder.rs::resource_outputs_inherit_class_and_id` |
| Parallel branches | covered | `crates/core/src/graph/tests.rs::parallel_branches_share_a_layer`, `crates/engine/src/planner/tests.rs::independent_branches_run_in_parallel` |
| Database host reaches the workload | covered | `crates/server/src/tests/e2e.rs::a_function_with_a_database_and_a_queue_deploys_and_replays` (`DB_HOST: db.internal`), `crates/engine/src/planner/tests/workloads.rs::a_none_workload_stores_its_resolved_document` |
| Inspect after deployment | covered | `crates/server/src/tests/resources.rs::inspect_the_graph_after_a_failed_deployment` |
| Conflicting shared table | covered | `crates/core/src/graph/tests.rs::conflicting_shared_table_names_both_paths`, `crates/server/src/tests/resources.rs::conflicting_shared_parameters_provision_nothing` |

## secret-stores-and-references

| Scenario | Status | Verified by |
|---|---|---|
| Register an AWS store | deferred | Store check needs the AWS adapter (task 13.2); registration itself: `crates/server/src/tests/accounts.rs::register_an_aws_store` |
| Change primary | deferred | Task 13.2 (explicitly moved there by task 3.9) |
| Pinned version | covered | `crates/store/src/secrets/tests.rs::versions_are_kept_and_pinned` |
| Placeholder in a reference | covered | `crates/server/src/secret_inputs.rs::placeholder_in_reference_is_rejected` |
| Definition with references from two stores | deferred | Reading `my-sm` needs the external adapter (13.2); creating such a definition: `crates/server/src/tests/accounts.rs::register_an_aws_store` |
| Inline secret becomes a reference | covered | `crates/server/src/tests/catalog.rs::definition_secrets_are_stored_as_references`, `crates/store/src/secrets/tests.rs::inline_values_become_references` |
| Update creates a version | covered | `crates/server/src/tests/catalog.rs::definition_secrets_are_stored_as_references` |
| Secret output stored as a reference | covered | `crates/engine/src/executor/tests.rs::a_secret_output_is_stored_and_returned_as_a_reference` |
| Store unreachable | gap (partial) | Only cloud-account credentials naming an unsupported store are tested (`crates/engine/src/cloud/tests.rs::static_keys_come_from_the_secret_reference`). No executor test where a definition/value reference's store read fails and the resource error names store and reference; searched `unreachable`, `could not be read`, `SecretError::Store` in `crates/engine/src/executor/tests.rs`, `crates/engine/src/planner/tests.rs` |
| Delete a store in use | covered | `crates/server/src/tests/accounts.rs::delete_a_store_in_use`, `crates/server/src/tests/accounts.rs::delete_a_store_used_by_shared_values` |
| Reference copied between applications | covered | `crates/server/src/tests/secret_use.rs::reference_copied_between_applications` |
| Production reference in development | covered | `crates/server/src/tests/secret_use.rs::production_reference_in_development` |
| Failure leaves a variables file | gap (partial) | Run directories are private and removed after a run or a reaped timeout (`crates/drivers/src/opentofu/tests.rs::fake_run_passes_credentials_through_files_and_environment_only`, `crates/drivers/src/opentofu/tests.rs::a_timed_out_run_is_interrupted_and_reaped_before_the_next`). No test leaves `secrets.tfvars.json` behind after a server crash and checks owner-only mode, removal on recovery and omission from diagnostics; searched `secrets.tfvars`, `runs/`, `mode()` in `crates/drivers/src/opentofu/tests.rs` |
| Master key unavailable | gap (partial) | `crates/server/src/tests/mod.rs::missing_master_key_blocks_readiness_and_mutations`, `crates/server/src/tests/restore.rs::backup_and_restore_drill` (partial: the key file is deleted rather than a database restored without it, and a deployment is not shown refused) |
| Store depends on itself | covered | `crates/server/src/tests/accounts.rs::store_depends_on_itself` |
| Environment secret copied to app values | covered | `crates/server/src/tests/secret_use.rs::environment_secret_copied_to_app_values` |
| Restore after key rotation | gap (partial) | `crates/server/src/tests/keys.rs::rotate_the_master_key`, `crates/store/src/secrets/tests.rs::rotation_keeps_old_versions_readable` (partial: no backup holding old and new key IDs is restored; key handling is shown only on a restart) |

## shared-values

| Scenario | Status | Verified by |
|---|---|---|
| Environment override | covered | `crates/server/src/tests/values.rs::environment_override`, `crates/server/src/tests/e2e.rs::a_function_with_a_database_and_a_queue_deploys_and_replays` |
| Override without base | covered | `crates/server/src/tests/values.rs::override_without_base` |
| Secret in a composite string | gap (partial) | `crates/engine/src/executor/tests.rs::a_secret_in_a_composite_string_is_rendered_only_for_the_driver` (partial: nothing asserts conversion and the planner keep the unresolved `${values.API_TOKEN}` expression) |
| Deployment pins its version | covered | `crates/server/src/tests/e2e.rs::a_deployment_pins_its_versions_and_replay_refuses_missing_inputs` |
| Delete environment | covered | `crates/server/src/tests/values.rs::delete_environment` |

## workload-spec

| Scenario | Status | Verified by |
|---|---|---|
| Function workload | covered | `crates/core/src/workload/tests.rs::function_workload` |
| Both kinds | covered | `crates/core/src/workload/tests.rs::both_kinds_are_rejected` |
| Unknown field | covered | `crates/core/src/workload/tests.rs::unknown_field_is_named` |
| Web container | covered | `crates/core/src/workload/tests.rs::web_container_implies_http_trigger` |
| Zip function | covered | `crates/core/src/workload/tests.rs::deploy_time_artifact_is_required_and_supplied_by_name` |
| Image function | gap (partial) | `crates/core/src/workload/tests.rs::image_function`, `crates/core/src/workload/tests.rs::deploy_time_image_is_required` (partial: no test shows deploying a function image package requires `--image <name>=<ref>`) |
| Queue trigger on a dependency | covered | `crates/core/src/workload/tests.rs::queue_trigger_on_a_dependency`, `crates/core/src/graph/tests.rs::queue_trigger_dependency_is_in_the_graph`, `crates/engine/src/planner/tests/workloads.rs::a_lambda_class_workload_gets_its_resolved_document` |
| Trigger names a missing resource | covered | `crates/core/src/workload/tests.rs::trigger_names_missing_resource` |
| Unsupported trigger for the target | deferred | `openporch/kubernetes`, group 9 |
| Shared resource | covered | `crates/core/src/graph/tests.rs::shared_resource_is_one_node`, `crates/core/src/workload/tests.rs::private_and_shared_dependencies` |
| Connection string | covered | `crates/core/src/workload/tests.rs::connection_string_is_translated_and_marked_secret` |
| Unknown output | covered | `crates/core/src/workload/tests.rs::unknown_output_is_rejected` |
| Pod annotation | deferred | Kubernetes driver rendering, group 9 |
| Lambda class | covered | `crates/engine/src/planner/tests/workloads.rs::a_lambda_class_workload_gets_its_resolved_document` |
| No workload definition | covered | `crates/engine/src/planner/tests/workloads.rs::a_workload_without_a_definition_names_its_class_and_environment`, `crates/core/src/graph/tests.rs::no_workload_definition_names_workload_class_and_env` |
| Resolved outputs without a runtime | covered | `crates/engine/src/planner/tests/workloads.rs::a_none_workload_stores_its_resolved_document` |
| Plain Score file | covered | `crates/core/src/workload/tests.rs::plain_score_file` |
| Overlay adds a class | covered | `crates/core/src/workload/tests.rs::overlay_adds_class_and_triggers` |
| Override image from CI | covered | `crates/core/src/workload/tests.rs::image_override_wins_over_the_file` |
| Mutable artifact | covered | `crates/server/src/tests/e2e.rs::artifacts_without_a_fixed_identity_are_refused`, `crates/engine/src/executor/tests.rs::artifacts_are_pinned_to_their_current_version_with_the_workloads_account` |

## Totals

274 scenarios: 190 covered, 9 manual, 31 deferred, 44 gaps.
