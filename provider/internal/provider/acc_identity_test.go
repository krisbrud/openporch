package provider

import (
	"context"
	"fmt"
	"os"
	"regexp"
	"testing"

	"github.com/hashicorp/terraform-plugin-testing/compare"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/statecheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// tokenWorks checks that the token in state authenticates as user.
func tokenWorks(resourceName, user string) resource.TestCheckFunc {
	return func(s *terraform.State) error {
		rs, ok := s.RootModule().Resources[resourceName]
		if !ok {
			return fmt.Errorf("%s is not in state", resourceName)
		}
		token := rs.Primary.Attributes["token"]
		if token == "" {
			return fmt.Errorf("%s has no token", resourceName)
		}
		c, err := client.New(os.Getenv(EnvURL), os.Getenv(EnvOrg), token, "acceptance-test")
		if err != nil {
			return err
		}
		me, err := c.CurrentUser(context.Background())
		if err != nil {
			return fmt.Errorf("the token does not authenticate: %w", err)
		}
		if me.ID != user {
			return fmt.Errorf("the token authenticates as %s, want %s", me.ID, user)
		}
		return nil
	}
}

func TestAccServiceUserTokenAndRoles(t *testing.T) {
	c := apiClient(t)
	ci := randomID("acc-ci")
	app := randomID("acc-app")
	envType := randomID("acc-type")
	sameToken := statecheck.CompareValue(compare.ValuesSame())
	config := func(name, role, appRole, tokenID, expires string) string {
		return fmt.Sprintf(`
resource "openporch_user" "ci" {
  id   = %q
  name = %q
  role = %q
}

resource "openporch_service_user_token" "deploy" {
  user_id     = openporch_user.ci.id
  id          = %q
  description = "CI deployments"
  expires_at  = %q
}

resource "openporch_application" "app" {
  id = %q
}

resource "openporch_application_user" "ci" {
  app_id  = openporch_application.app.id
  user_id = openporch_user.ci.id
  role    = %q
}

resource "openporch_environment_type" "t" {
  id = %q
}

resource "openporch_environment_type_user" "ci" {
  env_type_id = openporch_environment_type.t.id
  user_id     = openporch_user.ci.id
}
`, ci, name, role, tokenID, expires, app, appRole, envType)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		CheckDestroy: func(*terraform.State) error {
			ctx := context.Background()
			if _, err := c.User(ctx, ci); !client.IsNotFound(err) {
				return fmt.Errorf("service user %s still exists: %v", ci, err)
			}
			if _, err := c.Role(ctx, "apps", app, ci); !client.IsNotFound(err) {
				return fmt.Errorf("application role still exists: %v", err)
			}
			if _, err := c.Role(ctx, "env-types", envType, ci); !client.IsNotFound(err) {
				return fmt.Errorf("environment type role still exists: %v", err)
			}
			return nil
		},
		Steps: []resource.TestStep{
			{
				Config: config("CI", "member", "developer", "deploy", "2099-01-01T00:00:00+01:00"),
				Check:  tokenWorks("openporch_service_user_token.deploy", ci),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_environment_type_user.ci", tfjsonpath.New("role"),
						knownvalue.StringExact("deployer")),
					statecheck.ExpectSensitiveValue("openporch_service_user_token.deploy", tfjsonpath.New("token")),
				},
			},
			{
				// Renaming the user and changing roles keeps the token.
				Config: config("CI bot", "manager", "owner", "deploy", "2099-01-01T00:00:00+01:00"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_user.ci", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("openporch_application_user.ci", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("openporch_service_user_token.deploy", plancheck.ResourceActionNoop),
					},
				},
				Check: tokenWorks("openporch_service_user_token.deploy", ci),
			},
			{
				// A token revoked outside OpenTofu leaves the state, and its ID stays taken.
				PreConfig: func() {
					if err := c.RevokeToken(context.Background(), ci, "deploy"); err != nil {
						t.Fatal(err)
					}
				},
				Config: config("CI bot", "manager", "owner", "deploy", "2099-01-01T00:00:00+01:00"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_service_user_token.deploy", plancheck.ResourceActionCreate),
					},
				},
				ExpectError: regexp.MustCompile("Token ID already used"),
			},
			{
				Config: config("CI bot", "manager", "owner", "deploy-2", "2099-01-01T00:00:00+01:00"),
				Check:  tokenWorks("openporch_service_user_token.deploy", ci),
			},
			{
				// A new expiry issues a new token, which needs a new ID.
				Config:      config("CI bot", "manager", "owner", "deploy-2", "2098-06-01T12:00:00Z"),
				ExpectError: regexp.MustCompile("Token needs a new ID"),
			},
			{
				Config: config("CI bot", "manager", "owner", "deploy-3", "2098-06-01T12:00:00Z"),
				ConfigStateChecks: []statecheck.StateCheck{
					sameToken.AddStateValue("openporch_service_user_token.deploy", tfjsonpath.New("token")),
				},
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_service_user_token.deploy", plancheck.ResourceActionReplace),
					},
				},
				Check: tokenWorks("openporch_service_user_token.deploy", ci),
			},
			{
				// The same expiry written with an offset changes nothing on the server.
				Config: config("CI bot", "manager", "owner", "deploy-3", "2098-06-01T14:00:00+02:00"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_service_user_token.deploy", plancheck.ResourceActionUpdate),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{
					sameToken.AddStateValue("openporch_service_user_token.deploy", tfjsonpath.New("token")),
				},
			},
			{
				ResourceName:      "openporch_user.ci",
				ImportState:       true,
				ImportStateId:     ci,
				ImportStateVerify: true,
			},
			{
				// An imported token has no value, and its expiry in UTC. Against a
				// configuration with an offset, that plans an in-place update that
				// only records the configured form, never a replacement.
				ResourceName:  "openporch_service_user_token.deploy",
				ImportState:   true,
				ImportStateId: ci + "/deploy-3",
				ImportStateCheck: func(states []*terraform.InstanceState) error {
					a := states[0].Attributes
					if a["expires_at"] != "2098-06-01T12:00:00Z" || a["token"] != "" || a["description"] != "CI deployments" {
						return fmt.Errorf("imported %v", a)
					}
					return nil
				},
			},
			{
				ResourceName:    "openporch_application_user.ci",
				ImportState:     true,
				ImportStateId:   app + "/" + ci,
				ImportStateKind: resource.ImportBlockWithID,
			},
			{
				ResourceName:    "openporch_environment_type_user.ci",
				ImportState:     true,
				ImportStateId:   envType + "/" + ci,
				ImportStateKind: resource.ImportBlockWithID,
			},
		},
	})
}
