package provider

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"regexp"
	"testing"

	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/statecheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"

	"github.com/krisbrud/openporch/provider/internal/client"
)

func TestAccResourceClass(t *testing.T) {
	c := apiClient(t)
	ctx := context.Background()
	// A custom type, whose ID contains a `/` that the import ID must handle.
	ty := os.Getenv(EnvOrg) + "/" + randomID("acc-type")
	err := c.Do(ctx, http.MethodPost, c.OrgPath("resources", "types"),
		map[string]any{"id": ty, "name": "Acceptance", "use": "direct"}, nil)
	if err != nil {
		t.Fatalf("creating type %s: %v", ty, err)
	}
	t.Cleanup(func() {
		_ = c.Do(context.Background(), http.MethodDelete, c.OrgPath("resources", "types", ty), nil, nil)
	})
	config := func(description string) string {
		return fmt.Sprintf(`
resource "openporch_resource_class" "large" {
  resource_type = %q
  id            = "large"
  description   = %q
}
`, ty, description)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			{
				Config: config("More CPU"),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_resource_class.large", tfjsonpath.New("description"),
						knownvalue.StringExact("More CPU")),
				},
			},
			{
				Config: config("More CPU and memory"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_resource_class.large", plancheck.ResourceActionReplace),
					},
				},
			},
			{
				ResourceName:                         "openporch_resource_class.large",
				ImportState:                          true,
				ImportStateId:                        ty + "/large",
				ImportStateVerify:                    true,
				ImportStateVerifyIdentifierAttribute: "id",
			},
		},
	})
}

// valueVersion reads the version of a secret value's reference.
func valueVersion(t *testing.T, c *client.Client, app, key string) string {
	t.Helper()
	v, err := c.Value(context.Background(), app, "", key)
	if err != nil {
		t.Fatal(err)
	}
	if v.SecretRef == nil {
		t.Fatalf("value %s is not a secret", key)
	}
	return v.SecretRef.Version
}

func TestAccValue(t *testing.T) {
	c := apiClient(t)
	app := randomID("acc-values")
	config := func(token, url string) string {
		return fmt.Sprintf(`
resource "openporch_application" "app" {
  id = %q
}

resource "openporch_value" "url" {
  app_id      = openporch_application.app.id
  key         = "API_URL"
  description = "Public API"
  value       = "https://api.dev.example"
}

resource "openporch_value" "url_dev" {
  app_id = openporch_application.app.id
  env_id = "development"
  key    = openporch_value.url.key
  value  = %q
}

resource "openporch_value" "token" {
  app_id       = openporch_application.app.id
  key          = "API_TOKEN"
  secret_value = %q
}

resource "openporch_value" "token_dev" {
  app_id       = openporch_application.app.id
  env_id       = "development"
  key          = openporch_value.token.key
  secret_value = "${%[3]q}-dev"
}

resource "openporch_value" "token_alias" {
  app_id     = openporch_application.app.id
  key        = "TOKEN_ALIAS"
  secret_ref = {
    store   = openporch_value.token.written_secret_ref.store
    ref     = openporch_value.token.written_secret_ref.ref
    version = openporch_value.token.written_secret_ref.version
  }
}
`, app, url, token)
	}
	written := func(version string) statecheck.StateCheck {
		return statecheck.ExpectKnownValue("openporch_value.token", tfjsonpath.New("written_secret_ref"),
			knownvalue.ObjectExact(map[string]knownvalue.Check{
				"store":   knownvalue.StringExact("openporch"),
				"ref":     knownvalue.StringExact("apps/" + app + "/values/API_TOKEN"),
				"version": knownvalue.StringExact(version),
			}))
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			{
				Config: config("one", "https://api.dev.example/v1"),
				ConfigStateChecks: []statecheck.StateCheck{
					written("1"),
					statecheck.ExpectKnownValue("openporch_value.token_dev",
						tfjsonpath.New("written_secret_ref").AtMapKey("version"), knownvalue.StringExact("1")),
				},
			},
			{
				// Changing another value keeps the secret's version.
				Config: config("one", "https://api.dev.example/v2"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_value.url_dev", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("openporch_value.token", plancheck.ResourceActionNoop),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("1")},
			},
			{
				Config: config("two", "https://api.dev.example/v2"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_value.token", plancheck.ResourceActionUpdate),
						plancheck.ExpectUnknownValue("openporch_value.token", tfjsonpath.New("written_secret_ref")),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("2")},
			},
			{
				// A secret written outside OpenTofu is written again.
				PreConfig: func() {
					other := "changed elsewhere"
					_, err := c.ReplaceValue(context.Background(), app, "", "API_TOKEN",
						client.ValueWrite{SecretRef: &client.SecretWrite{Value: &other}})
					if err != nil {
						t.Fatal(err)
					}
					if v := valueVersion(t, c, app, "API_TOKEN"); v != "3" {
						t.Fatalf("version %s after the outside write, want 3", v)
					}
				},
				Config: config("two", "https://api.dev.example/v2"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_value.token", plancheck.ResourceActionUpdate),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("4")},
			},
			{
				ResourceName:    "openporch_value.url_dev",
				ImportState:     true,
				ImportStateId:   app + "/development/API_URL",
				ImportStateKind: resource.ImportBlockWithID,
			},
			{
				ResourceName:    "openporch_value.token_alias",
				ImportState:     true,
				ImportStateId:   app + "/TOKEN_ALIAS",
				ImportStateKind: resource.ImportBlockWithID,
			},
			{
				// A secret value is imported as its reference.
				ResourceName:  "openporch_value.token_dev",
				ImportState:   true,
				ImportStateId: app + "/development/API_TOKEN",
				ImportStateCheck: func(states []*terraform.InstanceState) error {
					if len(states) != 1 {
						return fmt.Errorf("imported %d values", len(states))
					}
					a := states[0].Attributes
					if a["secret_ref.store"] != "openporch" || a["secret_ref.ref"] == "" || a["secret_value"] != "" {
						return fmt.Errorf("imported %v", a)
					}
					return nil
				},
			},
		},
	})
}

func TestAccInvalidSecretConfig(t *testing.T) {
	invalid := func(body, message string) resource.TestStep {
		return resource.TestStep{Config: body, PlanOnly: true, ExpectError: regexp.MustCompile(message)}
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			invalid(`
resource "openporch_value" "v" {
  app_id       = "shop"
  key          = "API_TOKEN"
  value        = "plain"
  secret_value = "secret"
}
`, "Set exactly one of"),
			invalid(`
resource "openporch_value" "v" {
  app_id = "shop"
  key    = "API_TOKEN"
}
`, "Set exactly one of"),
			invalid(`
resource "openporch_value" "v" {
  app_id     = "shop"
  key        = "API_TOKEN"
  secret_ref = { store = "openporch", ref = "apps/shop/values/API_TOKEN", version = "" }
}
`, "Empty value"),
			invalid(`
resource "openporch_cloud_account" "a" {
  id      = "aws-dev"
  type    = "aws"
  config  = { access_key_id = "AKIAIOSFODNN7EXAMPLE" }
  secrets = { secret_access_key = "secret" }
  secret_refs = {
    secret_access_key = { store = "openporch", ref = "accounts/other/secret_access_key" }
  }
}
`, "Duplicate secret"),
		},
	})
}

// accountVersion reads the version of a cloud account secret's reference.
func accountVersion(t *testing.T, c *client.Client, id, key string) string {
	t.Helper()
	a, err := c.Account(context.Background(), id)
	if err != nil {
		t.Fatal(err)
	}
	return a.SecretRefs[key].Version
}

func TestAccCloudAccount(t *testing.T) {
	c := apiClient(t)
	id := randomID("acc-aws")
	roleID := randomID("acc-role")
	aliasID := randomID("acc-alias")
	// The alias account references the other account's secret, which needs a grant.
	var grant struct {
		ID string `json:"id"`
	}
	err := c.Do(context.Background(), http.MethodPost, c.OrgPath("secretstores", "openporch", "grants"),
		map[string]any{"ref": "accounts/" + id + "/secret_access_key", "org_wide": true}, &grant)
	if err != nil {
		t.Fatalf("granting the secret: %v", err)
	}
	t.Cleanup(func() {
		_ = c.Do(context.Background(), http.MethodDelete,
			c.OrgPath("secretstores", "openporch", "grants", grant.ID), nil, nil)
	})
	config := func(region, secret string) string {
		return fmt.Sprintf(`
resource "openporch_cloud_account" "keys" {
  id   = %q
  type = "aws"
  config = {
    access_key_id = "AKIAIOSFODNN7EXAMPLE"
    region        = %q
  }
  secrets = {
    secret_access_key = %q
  }
}

resource "openporch_cloud_account" "alias" {
  id     = %[5]q
  type   = "aws"
  config = { access_key_id = "AKIAIOSFODNN7EXAMPLE" }
  secret_refs = {
    secret_access_key = {
      store = "openporch"
      ref   = openporch_cloud_account.keys.written_secret_refs["secret_access_key"].ref
    }
  }
}

resource "openporch_cloud_account" "role" {
  id   = %[4]q
  name = "Production role"
  type = "aws-role"
  config = {
    role_arn         = "arn:aws:iam::111122223333:role/openporch"
    external_id      = "a-long-random-value"
    session_duration = 7200
  }
}
`, id, region, secret, roleID, aliasID)
	}
	written := func(version string) statecheck.StateCheck {
		return statecheck.ExpectKnownValue("openporch_cloud_account.keys",
			tfjsonpath.New("written_secret_refs").AtMapKey("secret_access_key").AtMapKey("version"),
			knownvalue.StringExact(version))
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{
			{
				Config: config("eu-north-1", "first-secret"),
				ConfigStateChecks: []statecheck.StateCheck{
					written("1"),
					statecheck.ExpectKnownValue("openporch_cloud_account.keys", tfjsonpath.New("name"),
						knownvalue.StringExact(id)),
				},
			},
			{
				// A configuration change keeps the secret's version.
				Config: config("eu-west-1", "first-secret"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_cloud_account.keys", plancheck.ResourceActionUpdate),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("1")},
			},
			{
				Config: config("eu-west-1", "second-secret"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectUnknownValue("openporch_cloud_account.keys", tfjsonpath.New("written_secret_refs")),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("2")},
			},
			{
				// A secret written outside OpenTofu is written again.
				PreConfig: func() {
					other := "changed-elsewhere"
					_, err := c.ReplaceAccount(context.Background(), id, client.AccountWrite{
						Name: id, Type: "aws",
						Config:     map[string]any{"access_key_id": "AKIAIOSFODNN7EXAMPLE", "region": "eu-west-1"},
						SecretRefs: map[string]client.SecretWrite{"secret_access_key": {Value: &other}},
					})
					if err != nil {
						t.Fatal(err)
					}
					if v := accountVersion(t, c, id, "secret_access_key"); v != "3" {
						t.Fatalf("version %s after the outside write, want 3", v)
					}
				},
				Config: config("eu-west-1", "second-secret"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_cloud_account.keys", plancheck.ResourceActionUpdate),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{written("4")},
			},
			{
				ResourceName:      "openporch_cloud_account.alias",
				ImportState:       true,
				ImportStateId:     aliasID,
				ImportStateVerify: true,
			},
			{
				ResourceName:      "openporch_cloud_account.role",
				ImportState:       true,
				ImportStateId:     roleID,
				ImportStateVerify: true,
			},
			{
				// An imported secret is imported as its reference.
				ResourceName:            "openporch_cloud_account.keys",
				ImportState:             true,
				ImportStateId:           id,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"secrets", "written_secret_refs", "secret_refs"},
			},
		},
	})
}

func driverVersion(t *testing.T, c *client.Client, id string) string {
	t.Helper()
	d, err := c.Driver(context.Background(), id)
	if err != nil {
		t.Fatal(err)
	}
	if d.Token == nil {
		return ""
	}
	return d.Token.Version
}

func TestAccDriver(t *testing.T) {
	c := apiClient(t)
	id := "acc/" + randomID("driver")
	defID := randomID("acc-def")
	config := func(target, token string) string {
		return fmt.Sprintf(`
resource "openporch_driver" "d" {
  id     = %q
  target = %q
  token  = %q
  inputs_schema = {
    type     = "object"
    required = ["path"]
  }
  capabilities = {
    observe = true
  }
}

# A definition that uses the driver stays through token and target changes.
resource "openporch_resource_definition" "d" {
  id          = %q
  type        = "config"
  driver_type = openporch_driver.d.id
}
`, id, target, token, defID)
	}
	written := func(version string) statecheck.StateCheck {
		return statecheck.ExpectKnownValue("openporch_driver.d",
			tfjsonpath.New("written_token_ref").AtMapKey("version"), knownvalue.StringExact(version))
	}
	updated := []plancheck.PlanCheck{
		plancheck.ExpectResourceAction("openporch_driver.d", plancheck.ResourceActionUpdate),
		plancheck.ExpectResourceAction("openporch_resource_definition.d", plancheck.ResourceActionNoop),
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		CheckDestroy: func(*terraform.State) error {
			if _, err := c.Driver(context.Background(), id); !client.IsNotFound(err) {
				return fmt.Errorf("driver %s still exists: %v", id, err)
			}
			return nil
		},
		Steps: []resource.TestStep{
			{
				// The server normalizes the target, resolving dot segments Go does not;
				// the configured one is kept.
				Config: config("https://Drivers.ACME.example:443/a/../vault", "first-token"),
				ConfigStateChecks: []statecheck.StateCheck{
					written("1"),
					statecheck.ExpectKnownValue("openporch_driver.d", tfjsonpath.New("target"),
						knownvalue.StringExact("https://Drivers.ACME.example:443/a/../vault")),
					statecheck.ExpectKnownValue("openporch_driver.d", tfjsonpath.New("protocol_version"),
						knownvalue.StringExact("openporch-http/1")),
					statecheck.ExpectKnownValue("openporch_driver.d", tfjsonpath.New("cookie_version"),
						knownvalue.Int64Exact(1)),
					statecheck.ExpectKnownValue("openporch_driver.d",
						tfjsonpath.New("capabilities").AtMapKey("targets").AtMapKey("resources"), knownvalue.Bool(true)),
					statecheck.ExpectKnownValue("openporch_driver.d",
						tfjsonpath.New("capabilities").AtMapKey("observe"), knownvalue.Bool(true)),
				},
			},
			{
				// A new token is a new version, written in place.
				Config: config("https://Drivers.ACME.example:443/a/../vault", "second-token"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: append([]plancheck.PlanCheck{
						plancheck.ExpectUnknownValue("openporch_driver.d", tfjsonpath.New("written_token_ref")),
					}, updated...),
				},
				ConfigStateChecks: []statecheck.StateCheck{written("2")},
			},
			{
				// The target moves in place and the token keeps its version.
				Config: config("https://drivers-v2.acme.example/vault", "second-token"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: updated,
				},
				ConfigStateChecks: []statecheck.StateCheck{
					written("2"),
					statecheck.ExpectKnownValue("openporch_driver.d", tfjsonpath.New("target"),
						knownvalue.StringExact("https://drivers-v2.acme.example/vault")),
				},
			},
			{
				// A token written outside OpenTofu is written again.
				PreConfig: func() {
					other := "changed-elsewhere"
					_, err := c.ReplaceDriver(context.Background(), id, client.DriverWrite{
						Target: "https://drivers-v2.acme.example/vault",
						Token:  &client.SecretWrite{Value: &other},
						InputsSchema: map[string]any{
							"type": "object", "required": []string{"path"},
						},
						Capabilities: map[string]any{"observe": true},
					})
					if err != nil {
						t.Fatal(err)
					}
					if v := driverVersion(t, c, id); v != "3" {
						t.Fatalf("version %s after the outside write, want 3", v)
					}
				},
				Config: config("https://drivers-v2.acme.example/vault", "second-token"),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: updated,
				},
				ConfigStateChecks: []statecheck.StateCheck{written("4")},
			},
			{
				// The token is imported as its reference.
				ResourceName:            "openporch_driver.d",
				ImportState:             true,
				ImportStateId:           id,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"token", "token_ref", "written_token_ref"},
			},
			{
				// A reference to an earlier version of the driver's own token
				// cannot be fixed by a refresh, and the error says so.
				Config: fmt.Sprintf(`
resource "openporch_driver" "d" {
  id     = %q
  target = "https://drivers-v2.acme.example/vault"
  token_ref = {
    store   = "openporch"
    ref     = "drivers/%s/token"
    version = "1"
  }
  inputs_schema = {
    type     = "object"
    required = ["path"]
  }
  capabilities = {
    observe = true
  }
}

resource "openporch_resource_definition" "d" {
  id          = %q
  type        = "config"
  driver_type = openporch_driver.d.id
}
`, id, id, defID),
				ExpectError: regexp.MustCompile("Update `token_ref` to the driver's current token"),
			},
		},
	})
}

func TestAccResourceType(t *testing.T) {
	c := apiClient(t)
	id := os.Getenv(EnvOrg) + "/" + randomID("acc-flags")
	defID := randomID("acc-def")
	config := func(name, inputs string) string {
		return fmt.Sprintf(`
resource "openporch_resource_type" "flags" {
  id       = %q
  %s
  use      = "direct"
  category = "config"
  %s
  outputs_schema = {
    type = "object"
    properties = {
      values  = { type = "object", properties = { url = { type = "string" } } }
      secrets = { type = "object", properties = { sdk_key = { type = "string" } } }
    }
  }
}

# A class and a definition of the type stay through changes to it.
resource "openporch_resource_class" "large" {
  resource_type = openporch_resource_type.flags.id
  id            = "large"
}

resource "openporch_resource_definition" "flags" {
  id          = %q
  type        = openporch_resource_type.flags.id
  driver_type = "openporch/echo"
}
`, id, name, inputs, defID)
	}
	kept := []plancheck.PlanCheck{
		plancheck.ExpectResourceAction("openporch_resource_type.flags", plancheck.ResourceActionUpdate),
		plancheck.ExpectResourceAction("openporch_resource_class.large", plancheck.ResourceActionNoop),
		plancheck.ExpectResourceAction("openporch_resource_definition.flags", plancheck.ResourceActionNoop),
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		CheckDestroy: func(*terraform.State) error {
			if _, err := c.Type(context.Background(), id); !client.IsNotFound(err) {
				return fmt.Errorf("resource type %s still exists: %v", id, err)
			}
			return nil
		},
		Steps: []resource.TestStep{
			{
				// Left out, the name is the ID and the inputs schema the server's default.
				Config: config("", ""),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_resource_type.flags", tfjsonpath.New("name"),
						knownvalue.StringExact(id)),
					statecheck.ExpectKnownValue("openporch_resource_type.flags", tfjsonpath.New("inputs_schema"),
						knownvalue.Null()),
				},
			},
			{
				Config: config(`name = "Feature flags"`,
					`inputs_schema = { type = "object", properties = { project = { type = "string" } } }`),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: kept},
				Check: func(*terraform.State) error {
					ty, err := c.Type(context.Background(), id)
					if err != nil {
						return err
					}
					if ty.Name != "Feature flags" || ty.InputsSchema["properties"] == nil {
						return fmt.Errorf("unexpected type %+v", ty)
					}
					return nil
				},
			},
			{
				// A change outside OpenTofu is planned back.
				PreConfig: func() {
					err := c.Do(context.Background(), http.MethodPut, c.OrgPath("resources", "types", id),
						map[string]any{"name": "Changed", "use": "direct"}, nil)
					if err != nil {
						t.Fatal(err)
					}
				},
				Config: config(`name = "Feature flags"`,
					`inputs_schema = { type = "object", properties = { project = { type = "string" } } }`),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: kept},
			},
			{
				// Removing the name and inputs schema returns them to the defaults.
				Config:           config("", ""),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: kept},
				Check: func(*terraform.State) error {
					ty, err := c.Type(context.Background(), id)
					if err != nil {
						return err
					}
					if ty.Name != id || ty.InputsSchema["properties"] != nil {
						return fmt.Errorf("unexpected type %+v", ty)
					}
					return nil
				},
			},
			{
				ResourceName:      "openporch_resource_type.flags",
				ImportState:       true,
				ImportStateId:     id,
				ImportStateVerify: true,
			},
		},
	})
}

func TestAccResourceTypeRefusesBuiltIns(t *testing.T) {
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{{
			Config: `
resource "openporch_resource_type" "pg" {
  id  = "postgres"
  use = "direct"
}
`,
			ExpectError: regexp.MustCompile("built-in type `postgres`"),
		}, {
			Config: `
resource "openporch_resource_type" "bad" {
  id  = "acme/bad"
  use = "sometimes"
}
`,
			ExpectError: regexp.MustCompile("Invalid use"),
		}},
	})
}

func TestAccSecretStore(t *testing.T) {
	c := apiClient(t)
	app := randomID("acc-grants")
	account := randomID("acc-sm")
	store := randomID("acc-vault")
	config := func(region, primary string) string {
		return fmt.Sprintf(`
resource "openporch_application" "app" {
  id = %q
}

resource "openporch_cloud_account" "aws" {
  id      = %q
  type    = "aws"
  config  = { access_key_id = "AKIAIOSFODNN7EXAMPLE", region = "eu-north-1" }
  secrets = { secret_access_key = "secret" }
}

resource "openporch_secret_store" "vault" {
  id            = %q
  type          = "aws-secrets-manager"
  region        = %q
  cloud_account = openporch_cloud_account.aws.id
  %s
}

resource "openporch_secret_grant" "app" {
  store  = openporch_secret_store.vault.id
  ref    = "shop/stripe-key"
  app_id = openporch_application.app.id
  env_id = "development"
}

resource "openporch_secret_grant" "platform" {
  store    = "openporch"
  ref      = "platform/%[1]s"
  org_wide = true
}
`, app, account, store, region, primary)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		CheckDestroy: func(*terraform.State) error {
			if _, err := c.SecretStore(context.Background(), store); !client.IsNotFound(err) {
				return fmt.Errorf("secret store %s still exists: %v", store, err)
			}
			return nil
		},
		Steps: []resource.TestStep{
			{
				Config: config("eu-north-1", ""),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("openporch_secret_store.vault", tfjsonpath.New("primary"),
						knownvalue.Bool(false)),
					statecheck.ExpectKnownValue("openporch_secret_grant.app", tfjsonpath.New("org_wide"),
						knownvalue.Bool(false)),
					statecheck.ExpectKnownValue("openporch_secret_grant.platform", tfjsonpath.New("app_id"),
						knownvalue.Null()),
				},
			},
			{
				// The region changes in place and the grant on the store stays.
				Config: config("eu-west-1", ""),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_secret_store.vault", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("openporch_secret_grant.app", plancheck.ResourceActionNoop),
					},
				},
				Check: func(*terraform.State) error {
					s, err := c.SecretStore(context.Background(), store)
					if err != nil {
						return err
					}
					if s.Config["region"] != "eu-west-1" {
						return fmt.Errorf("unexpected store %+v", s)
					}
					return nil
				},
			},
			{
				// A grant deleted outside OpenTofu is created again.
				PreConfig: func() {
					ctx := context.Background()
					var grants []client.Grant
					if err := c.Do(ctx, http.MethodGet, c.OrgPath("secretstores", store, "grants"), nil, &grants); err != nil {
						t.Fatal(err)
					}
					for _, g := range grants {
						if err := c.DeleteGrant(ctx, store, g.ID); err != nil {
							t.Fatal(err)
						}
					}
				},
				Config: config("eu-west-1", ""),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("openporch_secret_grant.app", plancheck.ResourceActionCreate),
					},
				},
			},
			{
				ResourceName:      "openporch_secret_store.vault",
				ImportState:       true,
				ImportStateId:     store,
				ImportStateVerify: true,
			},
			{
				ResourceName: "openporch_secret_grant.app",
				ImportState:  true,
				ImportStateIdFunc: func(s *terraform.State) (string, error) {
					return store + "/" + s.RootModule().Resources["openporch_secret_grant.app"].Primary.ID, nil
				},
				ImportStateVerify: true,
			},
			{
				// Promotion opens the store with its account first; with keys AWS
				// refuses, nothing changes.
				Config:      config("eu-west-1", "primary = true"),
				ExpectError: regexp.MustCompile(`CLD-00[12]`),
			},
		},
	})
}

func TestAccSecretGrantNeedsAScope(t *testing.T) {
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { preCheck(t) },
		ProtoV6ProviderFactories: providers,
		Steps: []resource.TestStep{{
			Config: `
resource "openporch_secret_grant" "g" {
  store = "openporch"
  ref   = "platform/x"
}
`,
			ExpectError: regexp.MustCompile("Missing scope"),
		}, {
			Config: `
resource "openporch_secret_grant" "g" {
  store    = "openporch"
  ref      = "platform/x"
  app_id   = "shop"
  org_wide = true
}
`,
			ExpectError: regexp.MustCompile("Conflicting scope"),
		}},
	})
}
