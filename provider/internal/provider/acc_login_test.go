package provider

import (
	"context"
	"fmt"
	"testing"

	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/statecheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"

	"github.com/krisbrud/openporch/provider/internal/client"
)

func TestAccIdentityProvider(t *testing.T) {
	c := apiClient(t)
	id := randomID("acc-idp")
	renamed := id + "-2"
	var configFor func(id, secret, scopes string) string
	name := "openporch_identity_provider.corp"
	config := func(secret, scopes string) string {
		return configFor(id, secret, scopes)
	}
	configFor = func(id, secret, scopes string) string {
		s := ""
		if secret != "" {
			s = fmt.Sprintf("client_secret = %q", secret)
		}
		sc := ""
		if scopes != "" {
			sc = "scopes = " + scopes
		}
		return fmt.Sprintf(`
resource "openporch_identity_provider" "corp" {
  id           = %q
  issuer       = "https://login.corp.example"
  client_id    = "openporch"
  callback_url = "https://porch.corp.example/auth/oidc/%s/callback"
  return_urls  = ["https://porch.corp.example/"]
  %s
  %s
}
`, id, id, s, sc)
	}
	written := func(version string) statecheck.StateCheck {
		return statecheck.ExpectKnownValue(name,
			tfjsonpath.New("written_client_secret_ref").AtMapKey("version"), knownvalue.StringExact(version))
	}
	updated := []plancheck.PlanCheck{plancheck.ExpectResourceAction(name, plancheck.ResourceActionUpdate)}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		CheckDestroy: func(*terraform.State) error {
			for _, id := range []string{id, renamed} {
				if _, err := c.IdentityProvider(context.Background(), id); !client.IsNotFound(err) {
					return fmt.Errorf("identity provider %s still exists: %v", id, err)
				}
			}
			return nil
		},
		Steps: []resource.TestStep{
			{
				Config: config("first-secret", ""),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PostApplyPostRefresh: []plancheck.PlanCheck{plancheck.ExpectEmptyPlan()},
				},
				ConfigStateChecks: []statecheck.StateCheck{
					written("1"),
					statecheck.ExpectKnownValue(name, tfjsonpath.New("written_client_secret_ref").AtMapKey("ref"),
						knownvalue.StringExact("identity-providers/"+id+"/client_secret")),
					statecheck.ExpectKnownValue(name, tfjsonpath.New("scopes"), knownvalue.ListExact([]knownvalue.Check{
						knownvalue.StringExact("openid"), knownvalue.StringExact("email"), knownvalue.StringExact("profile"),
					})),
					statecheck.ExpectKnownValue(name, tfjsonpath.New("loopback_return"), knownvalue.Bool(false)),
				},
			},
			{
				// Settings change in place and the secret keeps its version.
				Config:            config("first-secret", `["openid", "email"]`),
				ConfigPlanChecks:  resource.ConfigPlanChecks{PreApply: updated},
				ConfigStateChecks: []statecheck.StateCheck{written("1")},
			},
			{
				// A new secret is a new version.
				Config: config("second-secret", `["openid", "email"]`),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: append([]plancheck.PlanCheck{
						plancheck.ExpectUnknownValue(name, tfjsonpath.New("written_client_secret_ref")),
					}, updated...),
				},
				ConfigStateChecks: []statecheck.StateCheck{written("2")},
			},
			{
				// A secret written outside OpenTofu is written again.
				PreConfig: func() {
					other := "changed-elsewhere"
					_, err := c.ReplaceIdentityProvider(context.Background(), id, client.IdentityProviderWrite{
						ID:           id,
						Issuer:       "https://login.corp.example",
						ClientID:     "openporch",
						CallbackURL:  "https://porch.corp.example/auth/oidc/" + id + "/callback",
						ReturnURLs:   []string{"https://porch.corp.example/"},
						Scopes:       []string{"openid", "email"},
						ClientSecret: &client.SecretWrite{Value: &other},
					})
					if err != nil {
						t.Fatal(err)
					}
				},
				Config:            config("second-secret", `["openid", "email"]`),
				ConfigPlanChecks:  resource.ConfigPlanChecks{PreApply: updated},
				ConfigStateChecks: []statecheck.StateCheck{written("4")},
			},
			{
				ResourceName:            name,
				ImportState:             true,
				ImportStateId:           id,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"client_secret"},
			},
			{
				// A new ID replaces the provider and writes its secret under the new ID.
				Config: configFor(renamed, "second-secret", `["openid", "email"]`),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction(name, plancheck.ResourceActionReplace),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{
					written("1"),
					statecheck.ExpectKnownValue(name, tfjsonpath.New("written_client_secret_ref").AtMapKey("ref"),
						knownvalue.StringExact("identity-providers/"+renamed+"/client_secret")),
				},
			},
			{
				// Without a secret it is a public client.
				Config: configFor(renamed, "", `["openid", "email"]`),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: updated,
					// Nothing, the computed reference included, plans again.
					PostApplyPostRefresh: []plancheck.PlanCheck{plancheck.ExpectEmptyPlan()},
				},
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue(name, tfjsonpath.New("written_client_secret_ref"), knownvalue.Null()),
				},
				Check: func(*terraform.State) error {
					p, err := c.IdentityProvider(context.Background(), renamed)
					if err != nil {
						return err
					}
					if p.ClientSecret != nil || p.Confidential {
						return fmt.Errorf("provider still has a secret: %+v", p)
					}
					return nil
				},
			},
		},
	})
}
