package provider

import (
	"context"
	"fmt"
	"net/http"
	"testing"

	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/statecheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"

	"github.com/krisbrud/openporch/provider/internal/client"
)

func TestAccResourceDefinitionWithCriteria(t *testing.T) {
	c := apiClient(t)
	id := randomID("acc-redis")
	envType := randomID("acc-staging")
	config := func(port int, password string) string {
		return fmt.Sprintf(`
resource "openporch_environment_type" "staging" {
  id = %q
}

resource "openporch_resource_definition" "redis" {
  id          = %q
  type        = "redis"
  driver_type = "openporch/opentofu"
  driver_inputs = {
    script = <<-EOT
      variable "name" {}
      variable "port" {}
      variable "password" { sensitive = true }
      output "host" { value = var.name }
      output "port" { value = var.port }
      output "password" {
        value     = var.password
        sensitive = true
      }
    EOT
    variables = {
      name = "$${context.app.id}-$${context.env.id}"
      port = %d
    }
  }
  driver_secrets = {
    variables = {
      password = %q
      upstream = "$${resources.postgres-instance.outputs.password}"
    }
  }
  provision = {
    "aws-policy" = { match_dependents = true }
  }
}

resource "openporch_resource_definition" "alias" {
  id          = "${openporch_resource_definition.redis.id}-alias"
  type        = "redis"
  driver_type = "openporch/opentofu"
  driver_inputs = {
    script = "variable \"password\" { sensitive = true }"
  }
  driver_secret_refs = {
    variables = {
      password = {
        store = "openporch"
        ref   = openporch_resource_definition.redis.written_secret_refs.variables.password.ref
      }
    }
  }
}

resource "openporch_resource_definition_criteria" "staging" {
  resource_definition_id = openporch_resource_definition.redis.id
  env_type               = openporch_environment_type.staging.id
}
`, envType, id, port, password)
	}
	written := func(version string) statecheck.StateCheck {
		return statecheck.ExpectKnownValue("openporch_resource_definition.redis",
			tfjsonpath.New("written_secret_refs").AtMapKey("variables").AtMapKey("password").AtMapKey("version"),
			knownvalue.StringExact(version))
	}
	stored := func(s *terraform.State) error {
		d, err := c.Definition(context.Background(), id)
		if err != nil {
			return err
		}
		vars, _ := d.DriverInputs.Values["variables"].(map[string]any)
		if got := vars["name"]; got != "${context.app.id}-${context.env.id}" {
			return fmt.Errorf("stored name %v", got)
		}
		if len(d.Criteria) != 1 || d.Criteria[0].EnvType != envType {
			return fmt.Errorf("stored criteria %+v", d.Criteria)
		}
		// The literal password is written as a reference; the placeholder stays a
		// template.
		templates, _ := d.DriverInputs.Secrets["variables"].(map[string]any)
		if len(templates) != 1 || templates["upstream"] != "${resources.postgres-instance.outputs.password}" {
			return fmt.Errorf("stored secret templates %v", templates)
		}
		return nil
	}
	// The alias definition references the other's secret, which needs a grant.
	var grant struct {
		ID string `json:"id"`
	}
	err := c.Do(context.Background(), http.MethodPost, c.OrgPath("secretstores", "openporch", "grants"),
		map[string]any{"ref": "defs/" + id + "/variables/password", "org_wide": true}, &grant)
	if err != nil {
		t.Fatalf("granting the secret: %v", err)
	}
	t.Cleanup(func() {
		_ = c.Do(context.Background(), http.MethodDelete,
			c.OrgPath("secretstores", "openporch", "grants", grant.ID), nil, nil)
	})
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		CheckDestroy: func(*terraform.State) error {
			if _, err := c.Definition(context.Background(), id); !client.IsNotFound(err) {
				return fmt.Errorf("definition %s still exists: %v", id, err)
			}
			return nil
		},
		Steps: []resource.TestStep{
			{
				Config:            config(6379, "first"),
				Check:             stored,
				ConfigStateChecks: []statecheck.StateCheck{written("1")},
			},
			{
				// A plain input change keeps the secret's version.
				Config: config(6380, "first"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_resource_definition.redis", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("openporch_resource_definition_criteria.staging", plancheck.ResourceActionNoop),
					},
				},
				Check:             stored,
				ConfigStateChecks: []statecheck.StateCheck{written("1")},
			},
			{
				Config: config(6380, "second"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectUnknownValue("openporch_resource_definition.redis", tfjsonpath.New("written_secret_refs")),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("2")},
			},
			{
				// A secret written outside OpenTofu is written again.
				PreConfig: func() {
					ctx := context.Background()
					d, err := c.Definition(ctx, id)
					if err != nil {
						t.Fatal(err)
					}
					d.DriverInputs.SecretRefs = map[string]any{
						"variables": map[string]any{"password": map[string]any{"value": "changed elsewhere"}},
					}
					_, err = c.ReplaceDefinition(ctx, id, client.DefinitionWrite{
						Name: d.Name, DriverType: d.DriverType, DriverInputs: d.DriverInputs, Provision: d.Provision,
					})
					if err != nil {
						t.Fatal(err)
					}
				},
				Config: config(6380, "second"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_resource_definition.redis", plancheck.ResourceActionUpdate),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("4")},
			},
			{
				ResourceName:      "openporch_resource_definition.alias",
				ImportState:       true,
				ImportStateId:     id + "-alias",
				ImportStateVerify: true,
			},
			{
				// A written secret is imported as its reference, and co-provisioning
				// entries with the defaults the server fills in.
				ResourceName:      "openporch_resource_definition.redis",
				ImportState:       true,
				ImportStateId:     id,
				ImportStateVerify: true,
				ImportStateVerifyIgnore: []string{
					"driver_secrets", "written_secret_refs", "driver_secret_refs", "provision",
				},
			},
			{
				ResourceName: "openporch_resource_definition_criteria.staging",
				ImportState:  true,
				ImportStateIdFunc: func(s *terraform.State) (string, error) {
					rs := s.RootModule().Resources["openporch_resource_definition_criteria.staging"]
					return id + "/" + rs.Primary.Attributes["id"], nil
				},
				ImportStateKind: resource.ImportBlockWithID,
			},
		},
	})
}
