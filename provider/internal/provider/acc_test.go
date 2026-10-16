package provider

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"regexp"
	"testing"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/acctest"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/statecheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// Acceptance tests run with TF_ACC set, against the server that OPENPORCH_URL,
// OPENPORCH_ORG and OPENPORCH_TOKEN name (an administrator's token), with
// OpenTofu as TF_ACC_TERRAFORM_PATH. scripts/acceptance.sh sets this up.

var providers = map[string]func() (tfprotov6.ProviderServer, error){
	"openporch": providerserver.NewProtocol6WithError(New("test")()),
}

func preCheck(t *testing.T) {
	t.Helper()
	for _, v := range []string{EnvURL, EnvOrg, EnvToken} {
		if os.Getenv(v) == "" {
			t.Fatalf("%s must be set for acceptance tests", v)
		}
	}
}

// apiClient is a client for the acceptance test server, for setting up what the
// provider does not manage.
func apiClient(t *testing.T) *client.Client {
	t.Helper()
	if os.Getenv("TF_ACC") == "" {
		t.Skip("acceptance tests run with TF_ACC set")
	}
	preCheck(t)
	c, err := client.New(os.Getenv(EnvURL), os.Getenv(EnvOrg), os.Getenv(EnvToken), "acceptance-test")
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func randomID(prefix string) string {
	return prefix + "-" + acctest.RandStringFromCharSet(8, "abcdefghijklmnopqrstuvwxyz")
}

func TestAccEnvironmentType(t *testing.T) {
	id := randomID("acc-type")
	config := func(description string, protected bool) string {
		return fmt.Sprintf(`
resource "openporch_environment_type" "t" {
  id          = %q
  description = %q
  protected   = %t
}
`, id, description, protected)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			{
				Config: config("Live traffic", false),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_environment_type.t", tfjsonpath.New("description"),
						knownvalue.StringExact("Live traffic")),
					statecheck.ExpectKnownValue("openporch_environment_type.t", tfjsonpath.New("protected"),
						knownvalue.Bool(false)),
				},
			},
			{
				Config: config("Live customer traffic", true),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_environment_type.t", tfjsonpath.New("protected"),
						knownvalue.Bool(true)),
				},
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_environment_type.t", plancheck.ResourceActionUpdate),
					},
				},
			},
			{
				ResourceName:      "openporch_environment_type.t",
				ImportState:       true,
				ImportStateId:     id,
				ImportStateVerify: true,
			},
		},
	})
}

// deploy deploys a workload to app/env with definitions that match only app and
// use the `openporch/none` driver, and waits for it to succeed.
func deploy(t *testing.T, c *client.Client, app, env string) {
	t.Helper()
	ctx := context.Background()
	for _, ty := range []string{"base-env", "workload"} {
		id := app + "-" + ty
		body := map[string]any{
			"id": id, "type": ty, "driver_type": "openporch/none",
			"criteria": []map[string]string{{"app_id": app}},
		}
		err := c.Do(ctx, http.MethodPost, c.OrgPath("resources", "defs"), body, nil)
		var apiErr *client.Error
		if err != nil && !(asError(err, &apiErr) && apiErr.Status == http.StatusConflict) {
			t.Fatalf("creating definition %s: %v", id, err)
		}
		t.Cleanup(func() {
			_ = c.Do(context.Background(), http.MethodDelete, c.OrgPath("resources", "defs", id), nil, nil)
		})
	}
	var set struct {
		ID string `json:"id"`
	}
	delta := map[string]any{"modules": map[string]any{"add": map[string]any{
		"web": map[string]any{"class": "default", "spec": map[string]any{
			"containers": map[string]any{"web": map[string]any{"image": "nginx:1.27"}},
		}},
	}}}
	if err := c.Do(ctx, http.MethodPost, c.OrgPath("apps", app, "sets", "0"), delta, &set); err != nil {
		t.Fatalf("creating the set: %v", err)
	}
	e, err := c.Env(ctx, app, env)
	if err != nil {
		t.Fatal(err)
	}
	var d struct {
		ID     string `json:"id"`
		Status string `json:"status"`
	}
	body := map[string]any{"set_id": set.ID, "expected_generation": e.Generation}
	if err := c.Do(ctx, http.MethodPost, c.OrgPath("apps", app, "envs", env, "deploys"), body, &d); err != nil {
		t.Fatalf("deploying: %v", err)
	}
	for deadline := time.Now().Add(time.Minute); d.Status == "in_progress"; {
		if time.Now().After(deadline) {
			t.Fatalf("deployment %s did not finish", d.ID)
		}
		time.Sleep(200 * time.Millisecond)
		if err := c.Do(ctx, http.MethodGet, c.OrgPath("apps", app, "envs", env, "deploys", d.ID), nil, &d); err != nil {
			t.Fatal(err)
		}
	}
	if d.Status != "succeeded" {
		t.Fatalf("deployment %s: %s", d.ID, d.Status)
	}
}

func asError(err error, target **client.Error) bool {
	e, ok := err.(*client.Error)
	if ok {
		*target = e
	}
	return ok
}

func TestAccApplicationAndEnvironment(t *testing.T) {
	app := randomID("acc-app")
	envType := randomID("acc-type")
	c := apiClient(t)
	typeConfig := fmt.Sprintf(`
resource "openporch_environment_type" "staging" {
  id = %q
}
`, envType)
	appConfig := func(name, acks string) string {
		return fmt.Sprintf(`
resource "openporch_application" "app" {
  id = %q
  %s
  destroy_acknowledgements = [%s]
}
`, app, name, acks)
	}
	envConfig := func(name, acks string) string {
		return `
resource "openporch_environment" "staging" {
  app_id = openporch_application.app.id
  id     = "staging"
  type   = openporch_environment_type.staging.id
  ` + name + `
  destroy_acknowledgements = [` + acks + `]
}
`
	}
	const (
		base     = `"base-env.default#base-env=delete"`
		workload = `"workload.default#modules.web=delete"`
	)
	gone := func(what string, read func() error) resource.TestCheckFunc {
		return func(*terraform.State) error {
			if err := read(); !client.IsNotFound(err) {
				return fmt.Errorf("%s still exists: %v", what, err)
			}
			return nil
		}
	}
	// The error wraps lines, so only words are matched.
	refused := regexp.MustCompile(`(?s)nothing\s+was\s+deleted.*base-env\.default#base-env=delete`)
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			// Names default to the IDs.
			{
				Config: typeConfig + appConfig("", "") + envConfig("", ""),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_application.app", tfjsonpath.New("name"), knownvalue.StringExact(app)),
					statecheck.ExpectKnownValue("openporch_environment.staging", tfjsonpath.New("name"), knownvalue.StringExact("staging")),
				},
			},
			// Renames update in place.
			{
				Config: typeConfig + appConfig(`name = "Shop"`, "") + envConfig(`name = "Staging"`, ""),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_application.app", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("openporch_environment.staging", plancheck.ResourceActionUpdate),
					},
				},
			},
			{
				ResourceName:            "openporch_application.app",
				ImportState:             true,
				ImportStateId:           app,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"destroy_acknowledgements"},
			},
			{
				ResourceName:                         "openporch_environment.staging",
				ImportState:                          true,
				ImportStateId:                        app + "/staging",
				ImportStateVerify:                    true,
				ImportStateVerifyIdentifierAttribute: "id",
				ImportStateVerifyIgnore:              []string{"destroy_acknowledgements"},
			},
			// With resources deployed, removing the environment is refused until they
			// are acknowledged.
			{
				PreConfig:   func() { deploy(t, c, app, "staging") },
				Config:      typeConfig + appConfig(`name = "Shop"`, ""),
				ExpectError: refused,
			},
			{
				Config: typeConfig + appConfig(`name = "Shop"`, "") + envConfig(`name = "Staging"`, base+", "+workload),
			},
			{
				Config: typeConfig + appConfig(`name = "Shop"`, ""),
				Check: gone("the environment", func() error {
					_, err := c.Env(context.Background(), app, "staging")
					return err
				}),
			},
			// The same for the application, whose development environment now has
			// resources.
			{
				PreConfig:   func() { deploy(t, c, app, "development") },
				Config:      typeConfig,
				ExpectError: refused,
			},
			{
				Config: typeConfig + appConfig(`name = "Shop"`, `"development:base-env.default#base-env=delete", "development:workload.default#modules.web=delete"`),
			},
			{
				Config: typeConfig,
				Check: gone("the application", func() error {
					_, err := c.App(context.Background(), app)
					return err
				}),
			},
		},
	})
}

// An imported application keeps its name when the configuration sets none, so
// the plan after `tofu import` is empty.
func TestAccImportKeepsTheName(t *testing.T) {
	app := randomID("acc-app")
	c := apiClient(t)
	if _, err := c.CreateApp(context.Background(), app, "Imported shop"); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = c.DeleteApp(context.Background(), app, nil, "test cleanup") })
	// Import with a persisted state needs the provider's source spelled out.
	config := fmt.Sprintf(`
terraform {
  required_providers {
    openporch = { source = "openporch/openporch" }
  }
}

resource "openporch_application" "app" {
  id = %q
}
`, app)
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			{
				Config:             config,
				ResourceName:       "openporch_application.app",
				ImportState:        true,
				ImportStateId:      app,
				ImportStatePersist: true,
			},
			{
				Config: config,
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{plancheck.ExpectEmptyPlan()},
				},
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_application.app", tfjsonpath.New("name"),
						knownvalue.StringExact("Imported shop")),
				},
			},
			// A name the server would trim fails at plan.
			{
				Config: fmt.Sprintf(`
resource "openporch_application" "app" {
  id   = %q
  name = "Shop "
}
`, app),
				PlanOnly:    true,
				ExpectError: regexp.MustCompile(`without\s+surrounding\s+spaces`),
			},
			// The destroy after the test uses the last configuration.
			{Config: config},
		},
	})
}
